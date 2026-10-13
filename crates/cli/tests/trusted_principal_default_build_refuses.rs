//! A default build (no `trusted-principal` feature) must refuse
//! `daemon run --trusted-principal-config <path>` at argv-matching time,
//! not merely at runtime — `main.rs`'s hidden match arm for this argv
//! form is itself `#[cfg(feature = "trusted-principal")]`, so a default
//! build never compiles a code path that could reach trusted mode at
//! all. This is PR-2's design doc §10 acceptance criterion: "In a
//! default build, `libra-governor daemon run --trusted-principal-config
//! /x` exits 2."
//!
//! This test only makes sense — and only compiles meaningfully — when
//! run against a binary built *without* the feature, since a
//! feature-enabled `cargo test` run would hit the real `run_trusted`
//! entry point instead (which also exits non-zero for a nonexistent
//! config path, but for a different, feature-specific reason). It is
//! therefore itself gated the same way.

#![cfg(not(feature = "trusted-principal"))]

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

#[test]
fn default_build_exits_2_and_never_advertises_the_flag() {
    let output = Command::new(bin())
        .args(["daemon", "run", "--trusted-principal-config", "/x"])
        .output()
        .expect("spawn libra-governor");
    assert_eq!(
        output.status.code(),
        Some(2),
        "a default build must refuse this argv form with exit code 2, got: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown or missing subcommand"),
        "a default build falls through to the ordinary unknown-subcommand path, not a \
         trusted-principal-specific error (that would mean the feature leaked in): {stderr}"
    );

    // USAGE/--help must never mention the flag (design doc §8 "must NOT
    // advertise --trusted-principal-config in USAGE/--help").
    let help = Command::new(bin())
        .arg("--help")
        .output()
        .expect("spawn libra-governor --help");
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(
        !stdout.contains("--trusted-principal-config"),
        "--help must never advertise the hidden trusted-principal entry point: {stdout}"
    );
}
