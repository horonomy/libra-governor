use std::fs::{self, File};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use libra_governor_daemon::host_runtime::contract::{HostContract, ValidatedManifest};
use libra_governor_daemon::host_runtime::identity::measure_identity;
use libra_governor_daemon::host_runtime::RegistryEffect;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const BASE: &str =
    include_str!("../../protocol/contracts/host-adapter/v1/fixtures/valid-manifest-synthetic.json");

fn contract() -> &'static HostContract {
    static CONTRACT: OnceLock<HostContract> = OnceLock::new();
    CONTRACT.get_or_init(|| HostContract::load().unwrap())
}

fn write(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn declaration(path: &Path, kind: &str) -> Value {
    let hexadecimal: String = Sha256::digest(fs::read(path).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    json!({"path":path,"kind":kind,"digest":format!("sha256:{hexadecimal}")})
}

fn manifest(executable: &Path, argv: Vec<Value>, files: Vec<Value>) -> ValidatedManifest {
    let mut raw: Value = serde_json::from_str(BASE).unwrap();
    raw["launch"] = json!({"executable":executable,"argv":argv});
    raw["runtime_files"] = json!(files);
    contract()
        .validate_manifest(&serde_json::to_vec(&raw).unwrap())
        .unwrap()
}

fn refusal(manifest: &ValidatedManifest, reason: &str) {
    let error = measure_identity(manifest).unwrap_err();
    assert_eq!(error.stage, "identity");
    assert_eq!(error.reason, reason);
    assert_eq!(error.effect, RegistryEffect::NoChange);
    assert!(!error.to_string().contains("/"));
}

#[test]
fn native_declared_closure_is_order_independent_but_arguments_and_bytes_matter() {
    let dir = TempDir::new().unwrap();
    let exe = write(dir.path(), "synthetic", b"native fixture never executed");
    let dependency = write(dir.path(), "dependency", b"one");
    let declarations = vec![
        declaration(&exe, "entrypoint"),
        declaration(&dependency, "dependency"),
    ];
    let original = manifest(&exe, vec![json!("literal argument")], declarations.clone());
    let digest = measure_identity(&original).unwrap();
    assert_eq!(digest.len(), 71);
    assert!(digest.starts_with("sha256:"));
    let reversed = declarations.into_iter().rev().collect();
    assert_eq!(
        digest,
        measure_identity(&manifest(&exe, vec![json!("literal argument")], reversed)).unwrap()
    );
    assert_ne!(
        digest,
        measure_identity(&manifest(
            &exe,
            vec![json!("other")],
            vec![
                declaration(&exe, "entrypoint"),
                declaration(&dependency, "dependency")
            ]
        ))
        .unwrap()
    );
    fs::write(&dependency, b"two").unwrap();
    refusal(&original, "identity_digest_mismatch");
    assert_ne!(
        digest,
        measure_identity(&manifest(
            &exe,
            vec![json!("literal argument")],
            vec![
                declaration(&exe, "entrypoint"),
                declaration(&dependency, "dependency")
            ]
        ))
        .unwrap()
    );
}

#[test]
fn python_requires_actual_declared_script_and_tracks_its_content() {
    let dir = TempDir::new().unwrap();
    let python = write(dir.path(), "python3.14t", b"interpreter fixture");
    let script = write(
        dir.path(),
        "adapter.py",
        b"#!a comment when passed to Python\nprint(1)",
    );
    let only_interpreter = manifest(
        &python,
        vec![json!(script)],
        vec![declaration(&python, "entrypoint")],
    );
    refusal(&only_interpreter, "identity_undeclared_entrypoint");
    let original = manifest(
        &python,
        vec![json!(script), json!("--script-option")],
        vec![
            declaration(&python, "entrypoint"),
            declaration(&script, "entrypoint"),
        ],
    );
    let before = measure_identity(&original).unwrap();
    fs::write(&script, b"print(2)").unwrap();
    refusal(&original, "identity_digest_mismatch");
    let updated = manifest(
        &python,
        vec![json!(script), json!("--script-option")],
        vec![
            declaration(&python, "entrypoint"),
            declaration(&script, "entrypoint"),
        ],
    );
    assert_ne!(before, measure_identity(&updated).unwrap());
}

#[test]
fn python_versions_refuse_code_module_stdin_and_relative_modes() {
    let dir = TempDir::new().unwrap();
    for name in [
        "python",
        "python3",
        "python3.12",
        "python3.14d",
        "python3.14t",
    ] {
        let python = write(dir.path(), name, b"interpreter fixture");
        for argv in [
            vec![],
            vec![json!("-c"), json!("print(1)")],
            vec![json!("-m"), json!("module")],
            vec![json!("-")],
            vec![json!("relative.py")],
        ] {
            refusal(
                &manifest(&python, argv, vec![declaration(&python, "entrypoint")]),
                "unsupported_invocation",
            );
        }
    }
}

#[test]
fn symlinked_venv_invocation_preserves_literal_locator_and_resolved_semantics() {
    let dir = TempDir::new().unwrap();
    let python = write(dir.path(), "python3.12", b"interpreter fixture");
    let alias = dir.path().join("venv-python");
    symlink(&python, &alias).unwrap();
    let script = write(dir.path(), "adapter.py", b"print(1)");
    let declarations = vec![
        declaration(&python, "entrypoint"),
        declaration(&script, "entrypoint"),
    ];
    let direct = measure_identity(&manifest(
        &python,
        vec![json!(script)],
        declarations.clone(),
    ))
    .unwrap();
    let through_alias = manifest(&alias, vec![json!(script)], declarations);
    assert_ne!(direct, measure_identity(&through_alias).unwrap());
    let shell = write(dir.path(), "sh", b"shell fixture");
    fs::remove_file(&alias).unwrap();
    symlink(&shell, &alias).unwrap();
    refusal(
        &manifest(&alias, vec![], vec![declaration(&shell, "entrypoint")]),
        "unsupported_invocation",
    );
    refusal(&through_alias, "identity_undeclared_entrypoint");
}

#[test]
fn direct_shebang_requires_one_declared_absolute_python_interpreter() {
    let dir = TempDir::new().unwrap();
    let python = write(dir.path(), "python3", b"interpreter fixture");
    let script = write(
        dir.path(),
        "adapter",
        format!("#!{}\nprint(1)\n", python.display()).as_bytes(),
    );
    let good = manifest(
        &script,
        vec![],
        vec![
            declaration(&script, "entrypoint"),
            declaration(&python, "entrypoint"),
        ],
    );
    assert!(measure_identity(&good).is_ok());
    refusal(
        &manifest(&script, vec![], vec![declaration(&script, "entrypoint")]),
        "identity_undeclared_entrypoint",
    );
    for line in [
        format!("#!{} -I\n", python.display()),
        "#!/usr/bin/env python3\n".into(),
        "#! python3\n".into(),
        "#!relative\n".into(),
        format!("#!{}\r\n", python.display()),
        format!("#!/{}\n", "x".repeat(256)),
    ] {
        fs::write(&script, line).unwrap();
        refusal(
            &manifest(
                &script,
                vec![],
                vec![
                    declaration(&script, "entrypoint"),
                    declaration(&python, "entrypoint"),
                ],
            ),
            "unsupported_invocation",
        );
    }
}

#[test]
fn nested_interpreter_shebangs_and_other_known_runtimes_are_refused() {
    let dir = TempDir::new().unwrap();
    let python = write(dir.path(), "python3", b"#!/another/interpreter\n");
    let script = write(
        dir.path(),
        "adapter",
        format!("#!{}\n", python.display()).as_bytes(),
    );
    refusal(
        &manifest(
            &script,
            vec![],
            vec![
                declaration(&script, "entrypoint"),
                declaration(&python, "entrypoint"),
            ],
        ),
        "unsupported_invocation",
    );
    refusal(
        &manifest(
            &python,
            vec![json!(script)],
            vec![
                declaration(&script, "entrypoint"),
                declaration(&python, "entrypoint"),
            ],
        ),
        "unsupported_invocation",
    );
    for name in ["env", "which", "bash", "zsh", "node", "ruby", "pypy3"] {
        let exe = write(dir.path(), name, b"never executed");
        refusal(
            &manifest(&exe, vec![], vec![declaration(&exe, "entrypoint")]),
            "unsupported_invocation",
        );
    }
}

#[test]
fn resolved_and_hardlink_aliases_cannot_duplicate_a_declared_file() {
    let dir = TempDir::new().unwrap();
    let exe = write(dir.path(), "native", b"never executed");
    let symlink_path = dir.path().join("symlink");
    let hardlink_path = dir.path().join("hardlink");
    symlink(&exe, &symlink_path).unwrap();
    fs::hard_link(&exe, &hardlink_path).unwrap();
    for alias in [&exe, &symlink_path, &hardlink_path] {
        refusal(
            &manifest(
                &exe,
                vec![],
                vec![
                    declaration(&exe, "entrypoint"),
                    declaration(alias, "dependency"),
                ],
            ),
            "identity_alias",
        );
    }
}

#[test]
fn directories_fifos_and_nonexecutable_launches_fail_without_reading_special_files() {
    let dir = TempDir::new().unwrap();
    let exe = write(dir.path(), "native", b"never executed");
    let fifo = dir.path().join("fifo");
    // Fixture creation only; the production measurement never launches a process.
    assert!(std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    for unsafe_path in [&fifo, dir.path()] {
        let record = json!({"path":unsafe_path,"kind":"dependency","digest":format!("sha256:{}", "0".repeat(64))});
        refusal(
            &manifest(&exe, vec![], vec![declaration(&exe, "entrypoint"), record]),
            "identity_unsafe_file",
        );
    }
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o644)).unwrap();
    refusal(
        &manifest(&exe, vec![], vec![declaration(&exe, "entrypoint")]),
        "unsupported_invocation",
    );
}

#[test]
fn symlink_loops_and_excessive_chains_are_bounded() {
    let dir = TempDir::new().unwrap();
    let exe = write(dir.path(), "native", b"never executed");
    let loop_path = dir.path().join("loop");
    symlink("loop", &loop_path).unwrap();
    let bad =
        json!({"path":loop_path,"kind":"entrypoint","digest":format!("sha256:{}", "0".repeat(64))});
    refusal(&manifest(&loop_path, vec![], vec![bad]), "identity_limit");
    let mut previous = exe.clone();
    for n in 0..41 {
        let link = dir.path().join(format!("link-{n}"));
        symlink(&previous, &link).unwrap();
        previous = link;
    }
    refusal(
        &manifest(&previous, vec![], vec![declaration(&exe, "entrypoint")]),
        "identity_limit",
    );
}

#[test]
fn nonnormalized_paths_and_interpolation_are_refused_without_expansion() {
    let dir = TempDir::new().unwrap();
    let exe = write(dir.path(), "native", b"never executed");
    for spelling in [
        format!("{}/./native", dir.path().display()),
        format!("{}//native", dir.path().display()),
        format!("{}/sub/../native", dir.path().display()),
    ] {
        refusal(
            &manifest(
                Path::new(&spelling),
                vec![],
                vec![declaration(&exe, "entrypoint")],
            ),
            "unsupported_invocation",
        );
    }
    for argument in ["$(touch nowhere)", "${HOME}", "`id`"] {
        refusal(
            &manifest(
                &exe,
                vec![json!(argument)],
                vec![declaration(&exe, "entrypoint")],
            ),
            "unsupported_invocation",
        );
    }
}

#[test]
fn file_count_and_sparse_oversize_limits_refuse_before_large_reads() {
    let dir = TempDir::new().unwrap();
    let exe = write(dir.path(), "native", b"never executed");
    refusal(
        &manifest(&exe, vec![], vec![declaration(&exe, "entrypoint"); 129]),
        "identity_limit",
    );
    let large = dir.path().join("large");
    File::create(&large)
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    let record =
        json!({"path":large,"kind":"dependency","digest":format!("sha256:{}", "0".repeat(64))});
    refusal(
        &manifest(&exe, vec![], vec![declaration(&exe, "entrypoint"), record]),
        "identity_limit",
    );
}

#[test]
fn executable_and_script_dependency_classification_cannot_substitute_for_entrypoints() {
    let dir = TempDir::new().unwrap();
    let exe = write(dir.path(), "native", b"never executed");
    refusal(
        &manifest(&exe, vec![], vec![declaration(&exe, "dependency")]),
        "identity_undeclared_entrypoint",
    );
    let python = write(dir.path(), "python3", b"interpreter fixture");
    refusal(
        &manifest(
            &python,
            vec![json!(exe)],
            vec![
                declaration(&python, "entrypoint"),
                declaration(&exe, "dependency"),
            ],
        ),
        "identity_undeclared_entrypoint",
    );
}
