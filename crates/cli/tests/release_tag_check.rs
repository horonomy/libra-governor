//! HORO-2162: `scripts/check-release-tag.sh` must refuse a tag that does not
//! match the version the workspace builds, and accept the one that does.

use std::process::{Command, Output};

fn script() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/check-release-tag.sh")
}

fn real_manifest() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml")
}

fn check(tag: &str, manifest: &std::path::Path) -> Output {
    Command::new(script())
        .arg(tag)
        .arg(manifest)
        .output()
        .expect("run scripts/check-release-tag.sh")
}

fn manifest_with(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Cargo.toml");
    std::fs::write(&path, body).unwrap();
    (dir, path)
}

const FIXTURE: &str = "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"1.2.3\"\nedition = \"2021\"\n\n[workspace.dependencies]\nserde = { version = \"1\" }\n";

#[test]
fn the_matching_tag_passes() {
    let (_dir, path) = manifest_with(FIXTURE);
    let output = check("v1.2.3", &path);
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn a_mismatched_tag_is_refused() {
    let (_dir, path) = manifest_with(FIXTURE);
    for tag in ["v1.2.4", "v1.2", "1.2.3", "v1.2.3-rc.1", "v0.0.3"] {
        let output = check(tag, &path);
        assert_eq!(output.status.code(), Some(1), "{tag}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("does not match"),
            "{tag}"
        );
    }
}

#[test]
fn a_dependency_version_is_never_mistaken_for_the_workspace_version() {
    // Only the dependency table carries a version here.
    let (_dir, path) = manifest_with(
        "[workspace]\nmembers = []\n\n[workspace.dependencies]\nserde = { version = \"1\" }\nversion = \"9.9.9\"\n",
    );
    let output = check("v9.9.9", &path);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn a_missing_tag_is_a_usage_error() {
    let (_dir, path) = manifest_with(FIXTURE);
    let output = Command::new(script()).arg("").arg(&path).output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn the_real_workspace_version_is_what_this_binary_reports() {
    let version = env!("CARGO_PKG_VERSION");
    let output = check(&format!("v{version}"), &real_manifest());
    assert!(output.status.success(), "{output:?}");
    let stale = check(&format!("v{version}.1"), &real_manifest());
    assert_eq!(stale.status.code(), Some(1));
}
