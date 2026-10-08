use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn script() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh")
}

#[test]
fn help_documents_hooks_only_without_running_installer() {
    let output = Command::new(script())
        .arg("--help")
        .output()
        .expect("run scripts/install.sh --help");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--hooks-only"), "{stdout}");
    assert!(stdout.contains("statusLine"), "{stdout}");
    assert!(stdout.contains("LIBRA_GOVERNOR_CLAUDE_DIR"), "{stdout}");
}

#[test]
fn script_forwards_hooks_only_to_installed_binary() {
    let sandbox = tempfile::tempdir().unwrap();
    let mock_bin_dir = sandbox.path().join("mock-bin");
    let cargo_home = sandbox.path().join("cargo-home");
    let claude_dir = sandbox.path().join("scoped-claude-settings");
    std::fs::create_dir_all(&mock_bin_dir).unwrap();
    std::fs::create_dir_all(cargo_home.join("bin")).unwrap();
    let log = sandbox.path().join("binary-args.log");

    let cargo = mock_bin_dir.join("cargo");
    std::fs::write(
        &cargo,
        r##"#!/bin/sh
mkdir -p "$CARGO_HOME/bin"
cat > "$CARGO_HOME/bin/libra-governor" <<'BINARY'
#!/bin/sh
printf '%s\n' "$*" >> "$MOCK_BIN_LOG"
if [ "$1" = doctor ]; then printf 'doctor passed\n'; fi
BINARY
chmod +x "$CARGO_HOME/bin/libra-governor"
"##,
    )
    .unwrap();
    let rustc = mock_bin_dir.join("rustc");
    std::fs::write(&rustc, "#!/bin/sh\nprintf 'rustc test mock\\n'\n").unwrap();
    for path in [&cargo, &rustc] {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    let path = format!("{}:/usr/bin:/bin", mock_bin_dir.display());
    let output = Command::new(script())
        .arg("--hooks-only")
        .env("PATH", path)
        .env("CARGO_HOME", &cargo_home)
        .env("LIBRA_GOVERNOR_CLAUDE_DIR", &claude_dir)
        .env("MOCK_BIN_LOG", &log)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run hooks-only installer with mock cargo and binary");

    assert!(
        output.status.success(),
        "installer failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&format!("{}/settings.json", claude_dir.display())));
    assert!(stdout.contains("statusLine was left unchanged"));
    assert!(!stdout.contains("the statusline should update"));
    assert_eq!(
        std::fs::read_to_string(log).unwrap(),
        "install --hooks-only\ndoctor\n"
    );
}

#[cfg(unix)]
#[test]
fn unknown_script_argument_fails_before_installing() {
    let output = Command::new(script())
        .arg("--statusline-only")
        .output()
        .expect("run installer with an unknown option");

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown argument"));
}
