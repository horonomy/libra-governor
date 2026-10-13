//! T6 — executable replacement, including PATH shadowing (ADR-0019 §3,
//! §6 PR-1).
//!
//! Defense: the provider binary sits on a root-owned path
//! (`/usr/local/libexec/libra-governor/`, `0755`, root-owned ancestors).
//! Only root launches the provider; nothing ever resolves it through
//! `PATH`. PR-1 proves both halves: the agent cannot overwrite the file
//! in place, and a hostile `PATH` entry never reaches an absolute-path
//! invocation.

use std::path::Path;

use libra_governor_two_principal_harness::{
    assert_disposable, file_identity, positive_control, reset_state_dirs, run_as, LG_AGENT,
    LG_PROVIDER, PROVIDER_BINARY, PROVIDER_STATE_DIR,
};

const PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// `mv` a replacement file over `target`, never writing in place —
/// writing in place against a binary another process currently has open
/// can fail with `ETXTBSY`, which would make a denied attack look
/// denied for the wrong reason. `mv`/`rename(2)` has no such failure
/// mode; what actually stops the two-principal case is `EACCES` on
/// creating/renaming an entry in a directory the agent cannot write to
/// — which is checked before any text-busy concern would ever apply.
fn attack_replace_via_mv(target: &str, agent_home: &str) -> std::process::Output {
    let tmp = format!("{agent_home}/replacement-binary");
    let write = run_as(
        &LG_AGENT,
        "sh",
        &[
            "-c",
            &format!("printf '#!/bin/sh\\necho pwned\\n' > {tmp} && chmod +x {tmp}"),
        ],
        &[("PATH", PATH_ENV), ("HOME", agent_home)],
    );
    assert!(
        write.status.success(),
        "setup: agent must be able to write its own replacement file"
    );

    run_as(
        &LG_AGENT,
        "mv",
        &["-f", &tmp, target],
        &[("PATH", PATH_ENV), ("HOME", agent_home)],
    )
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t06a_agent_cannot_overwrite_the_provider_binary() {
    assert_disposable();
    reset_state_dirs();

    // Sensitivity control: a long-running process started from the
    // agent's OWN copy of the binary, in a directory the agent owns,
    // can be mv-swapped out from under itself while running.
    let own_copy = "/home/lg-agent/libra-governor-copy";
    let cp = run_as(
        &LG_AGENT,
        "cp",
        &[PROVIDER_BINARY, own_copy],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        cp.status.success(),
        "setup: agent copying the (world-readable) binary must succeed"
    );
    let mut running = libra_governor_two_principal_harness::command_as(
        &LG_AGENT,
        own_copy,
        &["daemon", "run"],
        &[
            ("PATH", PATH_ENV),
            ("HOME", "/home/lg-agent"),
            (
                "LIBRA_GOVERNOR_STATE_DIR",
                "/home/lg-agent/sameuid-exe-state",
            ),
        ],
    )
    .spawn()
    .expect("spawn agent's own copy");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let sensitivity = attack_replace_via_mv(own_copy, "/home/lg-agent");
    assert!(
        sensitivity.status.success(),
        "sensitivity control: lg-agent must be able to mv-swap its OWN running binary; stderr={}",
        String::from_utf8_lossy(&sensitivity.stderr)
    );
    let _ = running.kill();
    let _ = running.wait();

    // Positive control + two-principal run.
    let (mut daemon, _task_id) = positive_control(&LG_PROVIDER, PROVIDER_STATE_DIR, "/root");
    let (before_sha, before_len) = file_identity(Path::new(PROVIDER_BINARY));

    let attack = attack_replace_via_mv(PROVIDER_BINARY, "/home/lg-agent");
    assert!(
        !attack.status.success(),
        "lg-agent must NOT be able to replace the provider binary"
    );

    let (after_sha, after_len) = file_identity(Path::new(PROVIDER_BINARY));
    assert_eq!(
        before_sha, after_sha,
        "provider binary content must be unchanged"
    );
    assert_eq!(before_len, after_len);
    assert!(
        daemon.is_alive(),
        "the provider process must still be running, untouched"
    );

    daemon.kill9_and_reap();
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t06b_path_shadowing_never_reaches_an_absolute_path_invocation() {
    assert_disposable();
    reset_state_dirs();

    let hostile_dir = "/home/lg-agent/hostile";
    let sentinel = format!("{hostile_dir}/shadow-ran");
    let mkdir = run_as(
        &LG_AGENT,
        "mkdir",
        &["-p", hostile_dir],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(mkdir.status.success());
    let write_shadow = run_as(
        &LG_AGENT,
        "sh",
        &[
            "-c",
            &format!(
                "printf '#!/bin/sh\\ntouch {sentinel}\\n' > {hostile_dir}/libra-governor && chmod +x {hostile_dir}/libra-governor"
            ),
        ],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(write_shadow.status.success());

    let hostile_path = format!("{hostile_dir}:{PATH_ENV}");

    // Sensitivity-style proof the shadow is live: a BARE (non-absolute)
    // lookup with the same hostile PATH, as the agent itself, does find
    // it. This is not "the attack succeeding" — it is proof the planted
    // shadow would be found by a relative lookup, so its absence below
    // is meaningful.
    let bare_lookup = run_as(
        &LG_AGENT,
        "libra-governor",
        &["daemon", "run"],
        &[("PATH", hostile_path.as_str()), ("HOME", "/home/lg-agent")],
    );
    assert!(
        bare_lookup.status.success(),
        "sensitivity proof: a bare relative lookup must resolve to the planted shadow"
    );
    assert!(
        Path::new(&sentinel).exists(),
        "sensitivity proof: the shadow binary must have run and left its sentinel"
    );
    std::fs::remove_file(&sentinel).unwrap();

    // Real invocation: the operator always uses the absolute path
    // (ADR-0019 §1 "Authentication, in both directions": "It is always
    // an absolute path and never PATH-resolved"). Even with the hostile
    // PATH polluting the environment, the shadow must never run.
    let (mut daemon, _task_id) = positive_control_with_path(&hostile_path);
    assert!(
        !Path::new(&sentinel).exists(),
        "the planted shadow binary must NEVER run when the real client uses an absolute path"
    );
    daemon.kill9_and_reap();
}

/// Like [`positive_control`], but with an explicit (hostile) `PATH`
/// passed through — everything still invoked by `PROVIDER_BINARY`'s
/// absolute path, never a bare `libra-governor` lookup.
fn positive_control_with_path(
    hostile_path: &str,
) -> (libra_governor_two_principal_harness::DaemonProcess, String) {
    use libra_governor_two_principal_harness::{query_live, run_as_with_stdin, DaemonProcess};

    reset_state_dirs();
    let envs: [(&str, &str); 3] = [
        ("PATH", hostile_path),
        ("HOME", "/root"),
        ("LIBRA_GOVERNOR_STATE_DIR", PROVIDER_STATE_DIR),
    ];
    let daemon = DaemonProcess::start_with_envs(
        &LG_PROVIDER,
        PROVIDER_STATE_DIR,
        &envs,
        std::time::Duration::from_secs(10),
    );

    let payload = serde_json::json!({
        "session_id": "harness-path-shadow-control",
        "cwd": PROVIDER_STATE_DIR,
        "prompt": "harness path shadow control preflight",
    })
    .to_string();
    let out = run_as_with_stdin(
        &LG_PROVIDER,
        PROVIDER_BINARY,
        &["hook", "user-prompt-submit"],
        &envs,
        &payload,
    );
    assert!(
        out.status.success(),
        "hook user-prompt-submit must still succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let task_id = query_live(
        PROVIDER_STATE_DIR,
        "SELECT task_id FROM tasks ORDER BY created_at DESC LIMIT 1;",
    )
    .into_iter()
    .next()
    .expect("a task row must exist");

    (daemon, task_id)
}
