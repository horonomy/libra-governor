//! End-to-end tests of the real `libra-governor run` launcher/queue
//! wrapper and `libra-governor launch status` (HORO-1766 PR2). Spawns
//! the real built binary as a subprocess against an isolated
//! `LIBRA_GOVERNOR_STATE_DIR`/`HOME`, exactly like
//! `quota_observation_cli.rs`/`doctor_uninstall_integration.rs` do for
//! their own subcommands — against a real daemon (spawned on demand by
//! the wrapper's own `ensure_daemon_connection`, via `support::DaemonState`).
//!
//! Every scenario that needs deterministic synchronization (not a sleep
//! race) uses either a FIFO the test itself opens/closes, or a marker
//! file the child writes and the test polls for under a generous bounded
//! timeout — never a fixed sleep as the thing actually being asserted.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod support;

const BOUNDED_WAIT: Duration = Duration::from_secs(10);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

/// One isolated daemon state dir + fake `HOME`, mirroring
/// `quota_observation_cli.rs`'s `Sandbox` pattern.
struct Sandbox {
    state: support::DaemonState,
    home: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            state: support::DaemonState::new(),
            home: tempfile::tempdir().expect("create sandbox HOME"),
        }
    }

    fn write_config(&self, json: &serde_json::Value) {
        std::fs::write(self.state.path().join("config.json"), json.to_string())
            .expect("write config.json");
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(bin());
        cmd.args(args)
            .env("LIBRA_GOVERNOR_STATE_DIR", self.state.path())
            .env("HOME", self.home.path());
        cmd
    }

    fn run_output(&self, args: &[&str]) -> std::process::Output {
        self.command(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn libra-governor {args:?}: {e}"))
    }

    fn spawn_run(&self, args: &[&str]) -> Child {
        self.command(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn libra-governor {args:?}: {e}"))
    }
}

fn launcher_config(capacity: u64) -> serde_json::Value {
    serde_json::json!({"launcher": {"pools": [{"alias": "agents", "capacity": capacity}]}})
}

fn wait_for<F: Fn() -> bool>(timeout: Duration, condition: F) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return condition();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_marker(path: &Path, timeout: Duration) -> bool {
    wait_for(timeout, || path.exists())
}

/// Waits (bounded) for a spawned child to exit, without ever hanging the
/// test suite on a regression that leaves a process running forever.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn mkfifo(path: &Path) {
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("spawn mkfifo");
    assert!(status.success(), "mkfifo {path:?} failed");
}

/// Opens a FIFO's write end (blocking until the reader on the other side
/// is present, which it always is here — the gated child already has it
/// open for read), writes one byte, and closes it. Unblocks exactly one
/// blocked `cat <fifo>` reader.
fn unblock_fifo(path: &Path) {
    std::fs::write(path, b"x").unwrap_or_else(|e| panic!("unblock fifo {path:?}: {e}"));
}

fn assert_no_enforced_success_claim(output: &std::process::Output) {
    for stream in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(stream);
        assert!(
            !text.contains("enforced_at_launch_boundary"),
            "unexpected success claim in output: {text}"
        );
    }
}

fn kill_pid_best_effort(pid: &str) {
    let _ = Command::new("kill").args(["-9", pid]).status();
}

// ---------------------------------------------------------------------
// AC1: capacity 1, two real wrapper processes — admission serializes.
// ---------------------------------------------------------------------

#[test]
fn ac1_second_wrapper_waits_for_the_first_to_release_its_slot() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&launcher_config(1));

    let m1 = sandbox.state.path().join("m1");
    let m2 = sandbox.state.path().join("m2");
    let f1 = sandbox.state.path().join("f1");
    let f2 = sandbox.state.path().join("f2");
    mkfifo(&f1);
    mkfifo(&f2);

    let script1 = format!("touch '{}'; cat '{}'", m1.display(), f1.display());
    let script2 = format!("touch '{}'; cat '{}'", m2.display(), f2.display());

    let mut w1 = sandbox.spawn_run(&["run", "--pool", "agents", "--", "sh", "-c", &script1]);
    assert!(
        wait_for_marker(&m1, BOUNDED_WAIT),
        "first wrapper's child never started"
    );

    let mut w2 = sandbox.spawn_run(&["run", "--pool", "agents", "--", "sh", "-c", &script2]);
    // The slot is held by w1's job; w2 must stay in the Admit/Wait loop.
    std::thread::sleep(Duration::from_millis(1200));
    assert!(
        !m2.exists(),
        "second wrapper's child must not start while the only slot is held"
    );

    unblock_fifo(&f1);
    let status1 = wait_for_exit(&mut w1, BOUNDED_WAIT).expect("first wrapper never exited");
    assert!(status1.success(), "first wrapper's job must exit 0");

    assert!(
        wait_for_marker(&m2, BOUNDED_WAIT),
        "second wrapper's child never started after the first released its slot"
    );
    unblock_fifo(&f2);
    let status2 = wait_for_exit(&mut w2, BOUNDED_WAIT).expect("second wrapper never exited");
    assert!(status2.success(), "second wrapper's job must exit 0");
}

// ---------------------------------------------------------------------
// AC2: every refusal path fails closed -- no marker, exit 75, no claim.
// ---------------------------------------------------------------------

fn assert_fails_closed(output: &std::process::Output, marker: &Path) {
    assert_eq!(
        output.status.code(),
        Some(75),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !marker.exists(),
        "the wrapper must never spawn the command when admission is refused"
    );
    assert_no_enforced_success_claim(output);
}

#[test]
fn ac2_opt_in_off_refuses_and_never_spawns() {
    let sandbox = Sandbox::new();
    // Deliberately no config.json: the gate is off by default.
    let marker = sandbox.state.path().join("never.marker");
    let script = format!("touch '{}'", marker.display());
    let output = sandbox.run_output(&["run", "--pool", "agents", "--", "sh", "-c", &script]);
    assert_fails_closed(&output, &marker);
}

#[test]
fn ac2_disabled_pool_refuses_and_never_spawns() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&serde_json::json!({
        "launcher": {"pools": [{"alias": "agents", "capacity": 1, "enabled": false}]}
    }));
    let marker = sandbox.state.path().join("never.marker");
    let script = format!("touch '{}'", marker.display());
    let output = sandbox.run_output(&["run", "--pool", "agents", "--", "sh", "-c", &script]);
    assert_fails_closed(&output, &marker);
}

#[test]
fn ac2_unknown_pool_alias_refuses_and_never_spawns() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&serde_json::json!({
        "launcher": {"pools": [{"alias": "other-alias", "capacity": 1}]}
    }));
    let marker = sandbox.state.path().join("never.marker");
    let script = format!("touch '{}'", marker.display());
    let output = sandbox.run_output(&["run", "--pool", "agents", "--", "sh", "-c", &script]);
    assert_fails_closed(&output, &marker);
}

/// Points the daemon's state dir under a directory this process cannot
/// write into, so `ensure_state_dir` fails inside the spawned daemon,
/// the daemon exits immediately without ever binding its socket, and
/// `ensure_daemon_connection`'s spawn-then-poll loop genuinely times out
/// -- the one case this binary can simulate for "no daemon to ask at
/// all" (`UnavailableReason::DaemonUnreachable`'s whole reason for
/// existing, per `protocol::launch`'s own docs).
fn locked_unwritable_state_dir() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().expect("create root for locked state dir");
    let locked = root.path().join("locked");
    std::fs::create_dir(&locked).expect("create locked dir");
    let mut perms = std::fs::metadata(&locked).unwrap().permissions();
    perms.set_mode(0o500); // r-x, no write: a child can never be created inside it.
    std::fs::set_permissions(&locked, perms).expect("lock down perms");
    let state_dir = locked.join("state");
    (root, state_dir)
}

#[test]
fn ac2_daemon_unreachable_refuses_and_never_spawns() {
    let (_root, state_dir) = locked_unwritable_state_dir();
    let home = tempfile::tempdir().unwrap();
    // The marker path must live somewhere writable, since state_dir itself
    // can never be created.
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("never.marker");
    let script = format!("touch '{}'", marker.display());

    let output = Command::new(bin())
        .args(["run", "--pool", "agents", "--", "sh", "-c", &script])
        .env("LIBRA_GOVERNOR_STATE_DIR", &state_dir)
        .env("HOME", home.path())
        .output()
        .expect("spawn libra-governor run");

    assert_fails_closed(&output, &marker);
}

// ---------------------------------------------------------------------
// AC3: no environment clearing, no HOME tree mutation, no reference to
// claude/codex wiring modules.
// ---------------------------------------------------------------------

fn fake_home_fixture(home: &Path) {
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude/settings.json"), r#"{"hooks": {}}"#).unwrap();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(home.join(".codex/hooks.json"), r#"{"hooks": []}"#).unwrap();
    std::fs::write(home.join("statusline.sh"), "#!/bin/sh\necho ok\n").unwrap();
    std::fs::create_dir_all(home.join(".cc-switch")).unwrap();
    std::fs::write(
        home.join(".cc-switch/config.json"),
        r#"{"active": "anthropic"}"#,
    )
    .unwrap();
}

fn collect_tree(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_tree(root, &path, out);
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            out.push((rel, std::fs::read(&path).unwrap_or_default()));
        }
    }
}

fn hash_tree(root: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut entries = Vec::new();
    collect_tree(root, root, &mut entries);
    entries.sort();
    let mut hasher = Sha256::new();
    for (rel, contents) in &entries {
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        hasher.update(contents);
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn ac3_home_tree_is_byte_identical_across_a_full_admit_run_release_cycle() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&launcher_config(1));
    fake_home_fixture(sandbox.home.path());

    let before = hash_tree(sandbox.home.path());

    let env_file = sandbox.state.path().join("env.json");
    let output = sandbox.run_output(&[
        "run",
        "--pool",
        "agents",
        "--",
        bin(),
        "__dump_env",
        env_file.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let after = hash_tree(sandbox.home.path());
    assert_eq!(
        before, after,
        "the run wrapper must never touch any file under HOME"
    );
}

#[test]
fn ac3_child_environment_is_byte_identical_to_the_wrapper_except_the_two_test_overrides() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&launcher_config(1));

    const MARKER_NAME: &str = "LIBRA_RUN_WRAPPER_AC3_ENV_MARKER_7f3c2a";
    std::env::set_var(MARKER_NAME, "present-for-ac3");
    // Captured with the marker present, before spawning -- this is
    // exactly the environment the wrapper process itself inherits.
    let expected_env: BTreeMap<String, String> = std::env::vars().collect();

    let env_file = sandbox.state.path().join("env.json");
    let output = sandbox.run_output(&[
        "run",
        "--pool",
        "agents",
        "--",
        bin(),
        "__dump_env",
        env_file.to_str().unwrap(),
    ]);
    std::env::remove_var(MARKER_NAME);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let child_env: BTreeMap<String, String> =
        serde_json::from_slice(&std::fs::read(&env_file).unwrap()).unwrap();

    for (key, value) in &expected_env {
        // HOME/LIBRA_GOVERNOR_STATE_DIR are deliberately overridden on the
        // wrapper's own Command for test isolation -- checked separately
        // below against the values the wrapper itself was given, not the
        // outer test process's own ambient values.
        if key == "HOME" || key == "LIBRA_GOVERNOR_STATE_DIR" {
            continue;
        }
        assert_eq!(
            child_env.get(key),
            Some(value),
            "env var {key:?} did not pass through to the child unchanged"
        );
    }
    assert_eq!(
        child_env.get("HOME").map(String::as_str),
        Some(sandbox.home.path().to_str().unwrap())
    );
    assert_eq!(
        child_env
            .get("LIBRA_GOVERNOR_STATE_DIR")
            .map(String::as_str),
        Some(sandbox.state.path().to_str().unwrap())
    );
}

/// Static guard: `run_cmd`/`launch_cmd` must never grow a reference to
/// the Claude/Codex settings-wiring modules -- a thin client talking to
/// the launch-gate has no business touching `~/.claude/settings.json`,
/// `~/.codex/hooks.json`, the statusline script, or the install flow.
#[test]
fn run_and_launch_cmd_never_reference_claude_or_codex_wiring_modules() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for relative in ["src/run_cmd.rs", "src/launch_cmd.rs"] {
        let path = root.join(relative);
        let contents =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        for forbidden in [
            "claude_settings",
            "codex_hooks_file",
            "install_cmd",
            "statusline",
        ] {
            assert!(
                !contents.contains(forbidden),
                "{relative} must never reference `{forbidden}`"
            );
        }
    }
}

// ---------------------------------------------------------------------
// AC4: SIGTERM forwarding releases the slot; decoys/escapees never get
// signaled; an ignoring child keeps its hold; SIGINT to the whole
// process group lets the child die normally.
// ---------------------------------------------------------------------

#[test]
fn ac4_sigterm_to_the_wrapper_forwards_once_and_releases_on_exit() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&launcher_config(1));

    let started = sandbox.state.path().join("started");
    let checkpoint = sandbox.state.path().join("checkpoint");
    let script = format!(
        "trap \"touch '{}'; exit 0\" TERM; touch '{}'; while true; do sleep 1; done",
        checkpoint.display(),
        started.display()
    );

    let mut w1 = sandbox.spawn_run(&["run", "--pool", "agents", "--", "sh", "-c", &script]);
    assert!(wait_for_marker(&started, BOUNDED_WAIT), "job never started");

    let status = Command::new("kill")
        .args(["-TERM", &w1.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "failed to signal the wrapper process");

    let exit_status = wait_for_exit(&mut w1, BOUNDED_WAIT).expect("wrapper never exited");
    assert!(exit_status.success(), "trapped child exited 0");
    assert!(
        checkpoint.exists(),
        "the wrapper must forward SIGTERM to its pinned child"
    );

    // The slot must be released: a fresh admit must succeed promptly.
    let marker2 = sandbox.state.path().join("m2");
    let script2 = format!("touch '{}'", marker2.display());
    let output2 = sandbox.run_output(&["run", "--pool", "agents", "--", "sh", "-c", &script2]);
    assert!(output2.status.success());
    assert!(
        marker2.exists(),
        "the slot was not released after SIGTERM -- the next admit was refused"
    );
}

#[test]
fn ac4_sigterm_to_the_wrapper_never_reaches_a_decoy_or_an_escapee() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&launcher_config(1));

    let decoy_marker = sandbox.state.path().join("decoy.marker");
    let mut decoy = Command::new("sh")
        .args([
            "-c",
            &format!(
                "trap \"touch '{}'; exit 0\" TERM; while true; do sleep 1; done",
                decoy_marker.display()
            ),
        ])
        .spawn()
        .expect("spawn decoy process outside any wrapper");

    let started = sandbox.state.path().join("started");
    let checkpoint = sandbox.state.path().join("checkpoint");
    let escapee_marker = sandbox.state.path().join("escapee.marker");
    let escapee_pid_file = sandbox.state.path().join("escapee.pid");
    let script = format!(
        "trap \"touch '{checkpoint}'; exit 0\" TERM; \
         (trap \"touch '{escapee_marker}'; exit 0\" TERM; sleep 30) & \
         echo $! > '{escapee_pid_file}'; \
         touch '{started}'; \
         while true; do sleep 1; done",
        checkpoint = checkpoint.display(),
        escapee_marker = escapee_marker.display(),
        escapee_pid_file = escapee_pid_file.display(),
        started = started.display(),
    );

    let mut w1 = sandbox.spawn_run(&["run", "--pool", "agents", "--", "sh", "-c", &script]);
    assert!(wait_for_marker(&started, BOUNDED_WAIT), "job never started");
    assert!(
        wait_for_marker(&escapee_pid_file, BOUNDED_WAIT),
        "escapee grandchild never recorded its own pid"
    );

    let status = Command::new("kill")
        .args(["-TERM", &w1.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    let exit_status = wait_for_exit(&mut w1, BOUNDED_WAIT).expect("wrapper never exited");
    assert!(exit_status.success());
    assert!(checkpoint.exists(), "the owned child must receive SIGTERM");

    // Give any (incorrect) broader signal delivery a moment to land before
    // asserting its absence.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !decoy_marker.exists(),
        "a decoy process started outside any wrapper must never be signaled"
    );
    assert!(
        !escapee_marker.exists(),
        "a detached grandchild must never be signaled by the wrapper's single-pid SIGTERM"
    );

    let escapee_pid = std::fs::read_to_string(&escapee_pid_file)
        .unwrap()
        .trim()
        .to_string();
    kill_pid_best_effort(&escapee_pid);
    let _ = decoy.kill();
    let _ = decoy.wait();
}

#[test]
fn ac4_a_child_that_ignores_sigterm_keeps_its_hold_and_keeps_running() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&launcher_config(1));

    let started = sandbox.state.path().join("started");
    let pid_file = sandbox.state.path().join("child.pid");
    let script = format!(
        "echo $$ > '{}'; trap '' TERM; touch '{}'; while true; do sleep 1; done",
        pid_file.display(),
        started.display()
    );

    let mut w1 = sandbox.spawn_run(&["run", "--pool", "agents", "--", "sh", "-c", &script]);
    assert!(wait_for_marker(&started, BOUNDED_WAIT), "job never started");
    assert!(wait_for_marker(&pid_file, BOUNDED_WAIT));

    let status = Command::new("kill")
        .args(["-TERM", &w1.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    // The child ignores SIGTERM: the wrapper must not escalate (no
    // SIGKILL) and must keep the slot held.
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        w1.try_wait().unwrap().is_none(),
        "the wrapper must keep waiting, never force-kill an ignoring child"
    );

    let status_output = sandbox.run_output(&["launch", "status"]);
    assert!(status_output.status.success());
    let rendered = String::from_utf8_lossy(&status_output.stdout);
    assert!(
        rendered.contains("active=1/1"),
        "an ignored SIGTERM must not drop the held slot: {rendered}"
    );

    // Clean up: kill the child directly (bypassing the wrapper, which
    // only ever sends one non-escalating SIGTERM), letting the wrapper's
    // own waitid loop observe the exit and release/reap normally.
    let child_pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .to_string();
    kill_pid_best_effort(&child_pid);
    let _ = wait_for_exit(&mut w1, BOUNDED_WAIT);
}

#[test]
fn ac4_sigint_to_the_whole_process_group_lets_the_child_exit_normally() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&launcher_config(1));

    let started = sandbox.state.path().join("started");
    // Deliberately no SIGINT trap: default disposition terminates the
    // child immediately, which is exactly what must happen here -- if the
    // wrapper had left SIGINT `SIG_IGN`'d, that disposition would survive
    // `execve` and this child would ignore it and hang forever instead.
    let script = format!(
        "touch '{}'; while true; do sleep 1; done",
        started.display()
    );

    // The wrapper is put in its OWN process group (distinct from this
    // test binary's), so `kill -INT -<pgid>` can safely target exactly
    // {wrapper, child} without also signaling the test harness itself --
    // modeling a real foreground shell job, where the terminal's own job
    // control already does this.
    let mut command = sandbox.command(&["run", "--pool", "agents", "--", "sh", "-c", &script]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut w1 = command
        .spawn()
        .expect("spawn wrapper in its own process group");
    let wrapper_pid = w1.id();

    assert!(wait_for_marker(&started, BOUNDED_WAIT), "job never started");

    let status = Command::new("kill")
        .args(["-INT", &format!("-{wrapper_pid}")])
        .status()
        .unwrap();
    assert!(
        status.success(),
        "failed to signal the wrapper's process group"
    );

    let exit_status = wait_for_exit(&mut w1, BOUNDED_WAIT).expect("wrapper never exited");
    assert_eq!(
        exit_status.code(),
        Some(128 + libc::SIGINT),
        "the child must die from the group-wide SIGINT with its default disposition, proving \
         the wrapper never left SIGINT ignored for it"
    );
}

// ---------------------------------------------------------------------
// AC5 (CI-feasible part): a process started outside the wrapper is
// invisible to the launch gate, both in reality and in what `Status`
// claims.
// ---------------------------------------------------------------------

#[test]
fn ac5_a_process_started_outside_the_wrapper_is_never_governed() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&launcher_config(1));

    let m1 = sandbox.state.path().join("m1");
    let f1 = sandbox.state.path().join("f1");
    mkfifo(&f1);
    let script1 = format!("touch '{}'; cat '{}'", m1.display(), f1.display());
    let mut w1 = sandbox.spawn_run(&["run", "--pool", "agents", "--", "sh", "-c", &script1]);
    assert!(wait_for_marker(&m1, BOUNDED_WAIT));

    let stub_marker = sandbox.state.path().join("stub.marker");
    let mut stub = Command::new("sh")
        .args(["-c", &format!("touch '{}'; sleep 5", stub_marker.display())])
        .spawn()
        .expect("spawn stub agent process directly, bypassing the wrapper entirely");
    assert!(
        wait_for_marker(&stub_marker, BOUNDED_WAIT),
        "a process Libra never admitted must still be free to run"
    );

    let status_output = sandbox.run_output(&["launch", "status"]);
    assert!(status_output.status.success());
    let rendered = String::from_utf8_lossy(&status_output.stdout);
    assert!(
        rendered.contains("active=1/1"),
        "Status must count only the wrapper-owned job: {rendered}"
    );
    assert!(
        rendered.contains("launcher_owned_direct_children_only"),
        "{rendered}"
    );

    let _ = stub.kill();
    let _ = stub.wait();
    unblock_fifo(&f1);
    let _ = wait_for_exit(&mut w1, BOUNDED_WAIT);
}
