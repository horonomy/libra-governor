//! Root `--help`/`--version` contract for the real `libra-governor` binary
//! (HORO-1614, HORO-1607's CLI Experience v1 contract). No daemon, no
//! state dir, no network — these are the side-effect-free informational
//! surfaces the contract requires.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

#[test]
fn help_flag_exits_zero_and_prints_usage() {
    for flag in ["--help", "-h", "help"] {
        let output = Command::new(bin())
            .arg(flag)
            .output()
            .expect("spawn libra-governor");
        assert!(output.status.success(), "{flag} should exit 0");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("libra-governor"),
            "{flag} output should name the product: {stdout}"
        );
        assert!(
            stdout.contains("Usage:"),
            "{flag} output should contain a Usage section: {stdout}"
        );
        assert!(
            stdout.contains("doctor"),
            "{flag} output should list real subcommands: {stdout}"
        );
    }
}

#[test]
fn version_flag_exits_zero_and_prints_cargo_pkg_version() {
    for flag in ["--version", "-V", "version"] {
        let output = Command::new(bin())
            .arg(flag)
            .output()
            .expect("spawn libra-governor");
        assert!(output.status.success(), "{flag} should exit 0");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let expected = format!("libra-governor {}", env!("CARGO_PKG_VERSION"));
        assert_eq!(
            stdout.trim(),
            expected,
            "{flag} should print the authoritative workspace version"
        );
    }
}

/// Anti-vacuity: an unknown subcommand must still fail closed (non-zero,
/// no panic/stack trace), distinct from the two success paths above.
#[test]
fn unknown_subcommand_exits_nonzero_without_panicking() {
    let output = Command::new(bin())
        .arg("not-a-real-command")
        .output()
        .expect("spawn libra-governor");
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "must not panic: {stderr}");
}
