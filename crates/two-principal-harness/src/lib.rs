//! Fixtures for the disposable two-principal security harness (ADR-0019,
//! HORO-1835). Test-only; this crate's tests run **only** inside the
//! container `harness/two-principal/run.sh` builds and drives
//! (`docker run --rm --network none`) — never on a developer's real
//! machine, and never in a `cargo test --workspace` run that does not
//! pass `--ignored` and the disposable marker.
//!
//! This crate deliberately has **no Cargo dependency** on
//! `libra-governor-cli` or `libra-governor-daemon` (see `Cargo.toml`'s
//! doc comment). It drives the already-built `libra-governor` binary
//! purely through [`std::process::Command`] and stdin JSON, exactly as
//! an external Outcome Provider would, and it reads the ledger by
//! shelling out to the system `sqlite3` CLI rather than linking
//! `rusqlite`. This is what keeps a future `trusted-principal` Cargo
//! feature on `cli`/`daemon` (PR-2/PR-3) from ever needing to be
//! unified onto this crate under `resolver = "2"`'s workspace feature
//! unification (ADR-0019 §1).
//!
//! PR-1 scope only: this crate drives **today's existing binary**,
//! unmodified, run under a different UID with a pinned state directory.
//! There is no `trusted-principal` feature, no `AttestOutcome`, and no
//! `trusted-principal.json` yet — those are PR-2/PR-3.

use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// One of the harness's OS principals (ADR-0019 §1, "Harness name"
/// column).
#[derive(Debug, Clone, Copy)]
pub struct Principal {
    pub name: &'static str,
    pub uid: u32,
    pub gid: u32,
}

/// Runs the daemon; owns the ledger, state and socket directories.
/// Mirrors production `libra-provider` (system user, no login shell, not
/// in any admin/sudo group).
pub const LG_PROVIDER: Principal = Principal {
    name: "lg-provider",
    uid: 10_001,
    gid: 10_001,
};

/// The attacker in every threat test. A normal, non-admin, non-sudoer
/// user, member of `lg-clients` only.
pub const LG_AGENT: Principal = Principal {
    name: "lg-agent",
    uid: 10_002,
    gid: 10_002,
};

/// Container-only convenience account with a NOPASSWD sudoers grant,
/// used solely as a T9 sensitivity control (to prove `sudo -n` itself
/// functions in this container, so `lg-agent`'s failure is attributable
/// to its own lack of a grant rather than `sudo` being absent or
/// globally broken here). Never mirrors any production account; see
/// `provision.sh` for the one-line sudoers drop-in that exists only for
/// this user, only inside this disposable container.
pub const LG_SUDOPROBE: Principal = Principal {
    name: "lg-sudoprobe",
    uid: 10_003,
    gid: 10_003,
};

/// Provider-owned state directory — pinned via `LIBRA_GOVERNOR_STATE_DIR`
/// for today's unmodified binary (ADR-0019 §1 filesystem layout table;
/// `/etc/libra-governor/trusted-principal.json` does not exist yet in
/// PR-1, so this env var is the only pinning mechanism available).
pub const PROVIDER_STATE_DIR: &str = "/var/lib/libra-governor";

/// Sensitivity-control mirror of today's actual single-account
/// deployment: the SAME layout, entirely owned by `lg-agent`. Any attack
/// that fails against [`PROVIDER_STATE_DIR`] must succeed here, or the
/// test is not proving what it claims to prove (ADR-0019 §5 "Per-test
/// pattern", pass 2).
pub const SAMEUID_STATE_DIR: &str = "/srv/lg-sameuid";

/// Root-owned install path for today's binary (ADR-0019 §1 filesystem
/// layout table).
pub const PROVIDER_BINARY: &str = "/usr/local/libexec/libra-governor/libra-governor";

/// Must be called first by every test in this crate (and by every
/// fixture entry point in this module). Panics loudly — never a silent
/// skip — if any of the three disposability gates is missing, so a
/// `cargo test --ignored` run that somehow escapes the container still
/// cannot proceed.
pub fn assert_disposable() {
    let marker = std::env::var("LG_TWO_PRINCIPAL_DISPOSABLE").unwrap_or_default();
    let in_container = Path::new("/.dockerenv").exists();
    let is_root = unsafe { getuid() } == 0;

    if marker != "1" || !in_container || !is_root {
        panic!(
            "two-principal-harness: refusing to run outside the disposable \
             container. LG_TWO_PRINCIPAL_DISPOSABLE={marker:?} (want \"1\"), \
             /.dockerenv present={in_container}, running as uid 0={is_root}. \
             This crate's tests create and destroy OS users and must never \
             run on a real host."
        );
    }
}

// A tiny, dependency-free `getuid()` — this crate takes no `libc`
// dependency (see `Cargo.toml`), so the one raw syscall it needs is
// declared directly.
extern "C" {
    fn getuid() -> u32;
}

/// Runs `program` with `args`, as `principal`, via
/// `setpriv --reuid --regid --init-groups --reset-env --no-new-privs`
/// (ADR-0019 §1 "Launch and startup checks"). `--reset-env` wipes the
/// environment, so every env var the command needs (including `PATH`,
/// which is otherwise gone entirely) must be named in `envs`.
///
/// This is an `exec`-only chain — `setpriv` execs its argument directly,
/// never through a shell — so the resulting process's pid is exactly
/// the pid this function's `Command` reports, which the `/proc`, `kill`
/// and `ptrace` tests depend on.
pub fn command_as(
    principal: &Principal,
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Command {
    command_as_impl(principal, program, args, envs, true)
}

/// Same as [`command_as`], but WITHOUT `--no-new-privs`. Used only by
/// the T9 sudo test: `setpriv --no-new-privs` blocks `sudo` from using
/// its own setuid-root bit at all, unconditionally — confirmed
/// empirically running this harness for real, where even
/// `lg-sudoprobe`'s legitimate, container-only NOPASSWD grant was
/// refused with sudo's own "the 'no new privileges' flag is set"
/// message, not a sudoers-policy denial. That would make the sudo
/// sensitivity control itself vacuous (it couldn't prove sudo policy
/// works here at all, only that NNP blocks sudo universally), and would
/// make `lg-agent`'s denial attributable to NNP rather than to having no
/// grant — exactly the "denied for the wrong reason" failure mode this
/// harness's sensitivity controls exist to catch. So the sudo test
/// alone runs both principals without NNP, and asserts on sudo's own
/// policy-denial text (not a bare non-zero exit) to confirm the denial
/// `lg-agent` sees really is "no such grant", not an environment
/// artifact. Every other test in this crate keeps `--no-new-privs` on.
pub fn command_as_allow_setuid(
    principal: &Principal,
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Command {
    command_as_impl(principal, program, args, envs, false)
}

fn command_as_impl(
    principal: &Principal,
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    no_new_privs: bool,
) -> Command {
    let mut cmd = Command::new("setpriv");
    cmd.arg(format!("--reuid={}", principal.uid))
        .arg(format!("--regid={}", principal.gid))
        .arg("--init-groups")
        .arg("--reset-env");
    if no_new_privs {
        cmd.arg("--no-new-privs");
    }
    cmd.arg("--").arg("env");
    for (k, v) in envs {
        cmd.arg(format!("{k}={v}"));
    }
    cmd.arg(program);
    cmd.args(args);
    cmd
}

/// Runs `program` as `principal` to completion and returns its
/// [`Output`]. Convenience wrapper around [`command_as`] for the common
/// case of a one-shot attack attempt.
pub fn run_as(
    principal: &Principal,
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Output {
    command_as(principal, program, args, envs)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {program} as {}: {e}", principal.name))
}

/// Like [`run_as`], but via [`command_as_allow_setuid`] — see that
/// function's docs for why the T9 sudo test alone needs this.
pub fn run_as_allow_setuid(
    principal: &Principal,
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Output {
    command_as_allow_setuid(principal, program, args, envs)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {program} as {}: {e}", principal.name))
}

/// Same as [`run_as`], but writes `stdin` to the child's stdin before
/// reading output — for `libra-governor`'s stdin-JSON subcommands
/// (`hook user-prompt-submit`, `outcome record`).
pub fn run_as_with_stdin(
    principal: &Principal,
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin: &str,
) -> Output {
    let mut cmd = command_as(principal, program, args, envs);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn {program} as {}: {e}", principal.name));
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin to child");
    child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("failed to wait on {program} as {}: {e}", principal.name))
}

/// A running `libra-governor daemon run` process, launched as a
/// principal via [`command_as`] (never detached — this struct owns the
/// [`Child`], so its pid is authoritative for every `/proc`, `kill` and
/// `ptrace` test).
pub struct DaemonProcess {
    child: Child,
    pub pid: u32,
    pub state_dir: PathBuf,
}

impl DaemonProcess {
    /// Starts `libra-governor daemon run` as `principal`, pinned to
    /// `state_dir` via `LIBRA_GOVERNOR_STATE_DIR`, and blocks until its
    /// Unix socket accepts a connection or `timeout` elapses.
    pub fn start(principal: &Principal, state_dir: &str, home: &str, timeout: Duration) -> Self {
        Self::start_with_envs(
            principal,
            state_dir,
            &[
                (
                    "PATH",
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                ),
                ("HOME", home),
                ("LIBRA_GOVERNOR_STATE_DIR", state_dir),
            ],
            timeout,
        )
    }

    /// Like [`start`](Self::start), but with the full environment given
    /// explicitly (`--reset-env` wipes everything else) — used by tests
    /// that need to pin a hostile or otherwise non-default `PATH` while
    /// still pointing `LIBRA_GOVERNOR_STATE_DIR` at `state_dir`
    /// themselves (the caller is responsible for including it in
    /// `envs`).
    pub fn start_with_envs(
        principal: &Principal,
        state_dir: &str,
        envs: &[(&str, &str)],
        timeout: Duration,
    ) -> Self {
        assert_disposable();
        let mut cmd = command_as(principal, PROVIDER_BINARY, &["daemon", "run"], envs);
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to start daemon as {}: {e}", principal.name));
        let pid = child.id();

        let socket_path = Path::new(state_dir).join("daemon.sock");
        let deadline = Instant::now() + timeout;
        loop {
            if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                panic!(
                    "daemon (pid {pid}, principal {}) never accepted a \
                     connection at {socket_path:?} within {timeout:?}",
                    principal.name
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        Self {
            child,
            pid,
            state_dir: PathBuf::from(state_dir),
        }
    }

    /// `true` iff the process has not exited. Uses `try_wait`, not
    /// `kill -0` — a zombie answers `kill -0` successfully, which would
    /// make a crashed daemon look alive.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Sends `SIGKILL` as root (the orchestrator — root is never an
    /// attacker in this harness) and reaps the process.
    pub fn kill9_and_reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        // Best-effort teardown so one test's daemon never leaks into the
        // next. Each test additionally calls `kill9_and_reap` explicitly
        // where the kill itself is part of what is being asserted.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A root-held, read-only view of the ledger's authoritative state,
/// taken by copying the SQLite file **and its `-wal`/`-shm` sidecars**
/// to a scratch directory before querying — querying the live file in
/// place risks leaving root-owned sidecars behind that the provider
/// cannot later open, and `-wal` can hold committed data the main file
/// does not yet reflect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerSnapshot {
    pub exists: bool,
    pub sha256: String,
    pub inode: u64,
    pub triggers: Vec<String>,
    pub outcome_attestation_count: i64,
    pub task_budget_renewal_count: i64,
}

impl LedgerSnapshot {
    /// Captures the ledger at `state_dir` as root. Panics on any I/O or
    /// `sqlite3` failure other than the ledger simply not existing yet
    /// (e.g. before the positive control has run).
    pub fn capture(state_dir: &Path) -> Self {
        let main = state_dir.join("ledger.sqlite3");
        if !main.exists() {
            return Self {
                exists: false,
                sha256: String::new(),
                inode: 0,
                triggers: Vec::new(),
                outcome_attestation_count: 0,
                task_budget_renewal_count: 0,
            };
        }

        use std::os::unix::fs::MetadataExt;
        let inode = std::fs::metadata(&main)
            .unwrap_or_else(|e| panic!("stat {main:?}: {e}"))
            .ino();

        let scratch = tempfile_dir();
        let scratch_db = scratch.join("ledger.sqlite3");
        for ext in ["", "-wal", "-shm"] {
            let src = state_dir.join(format!("ledger.sqlite3{ext}"));
            if src.exists() {
                std::fs::copy(&src, scratch.join(format!("ledger.sqlite3{ext}")))
                    .unwrap_or_else(|e| panic!("copy {src:?}: {e}"));
            }
        }

        // Checkpoint the copy (not the live file) so the hash and row
        // counts reflect everything committed to the WAL, not just the
        // main file's on-disk bytes.
        sqlite3_exec(&scratch_db, "PRAGMA wal_checkpoint(TRUNCATE);");

        let sha256 = sha256_file(&scratch_db);

        let triggers = sqlite3_query(
            &scratch_db,
            "SELECT name FROM sqlite_master WHERE type='trigger' ORDER BY name;",
        );

        let outcome_attestation_count =
            sqlite3_query(&scratch_db, "SELECT COUNT(*) FROM outcome_attestations;")
                .first()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

        let task_budget_renewal_count =
            sqlite3_query(&scratch_db, "SELECT COUNT(*) FROM task_budget_renewals;")
                .first()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

        let _ = std::fs::remove_dir_all(&scratch);

        Self {
            exists: true,
            sha256,
            inode,
            triggers,
            outcome_attestation_count,
            task_budget_renewal_count,
        }
    }
}

fn tempfile_dir() -> PathBuf {
    let dir = PathBuf::from(format!(
        "/tmp/two-principal-harness-snapshot-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {dir:?}: {e}"));
    dir
}

fn sqlite3_exec(db: &Path, sql: &str) {
    let out = Command::new("sqlite3")
        .arg(db)
        .arg(sql)
        .output()
        .unwrap_or_else(|e| panic!("sqlite3 exec failed: {e}"));
    if !out.status.success() {
        panic!(
            "sqlite3 exec {sql:?} against {db:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Runs `sql` against `db` with `sqlite3 -readonly` and returns each
/// resulting row as one `String` (sqlite3's default `-list` output, one
/// column per query here so no further splitting is needed).
fn sqlite3_query(db: &Path, sql: &str) -> Vec<String> {
    let out = Command::new("sqlite3")
        .arg("-readonly")
        .arg(db)
        .arg(sql)
        .output()
        .unwrap_or_else(|e| panic!("sqlite3 query failed: {e}"));
    if !out.status.success() {
        panic!(
            "sqlite3 query {sql:?} against {db:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect()
}

fn sha256_file(path: &Path) -> String {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("sha256sum failed: {e}"));
    if !out.status.success() {
        panic!(
            "sha256sum {path:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// sha256 + size of an arbitrary file (used for the provider
/// executable's identity in T6).
pub fn file_identity(path: &Path) -> (String, u64) {
    let sha256 = sha256_file(path);
    let size = std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {path:?}: {e}"))
        .len();
    (sha256, size)
}

/// The current value of `/proc/sys/kernel/yama/ptrace_scope`, or `None`
/// if Yama is not compiled in. Printed by every ptrace test's output so
/// a reviewer can see whether denial came from the UID boundary, from
/// Yama, or (in the sensitivity control) neither.
pub fn yama_ptrace_scope() -> Option<i32> {
    std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope")
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Removes and recreates both [`PROVIDER_STATE_DIR`] and
/// [`SAMEUID_STATE_DIR`] with the ownership and modes ADR-0019 §1
/// specifies, as root. Called at the start of every test so tests are
/// independent and serial (`--test-threads=1`) re-provisioning never
/// carries state from one test into the next.
pub fn reset_state_dirs() {
    assert_disposable();
    for (dir, owner_uid, owner_gid, mode) in [
        (
            PROVIDER_STATE_DIR,
            LG_PROVIDER.uid,
            LG_PROVIDER.gid,
            0o700u32,
        ),
        (SAMEUID_STATE_DIR, LG_AGENT.uid, LG_AGENT.gid, 0o700u32),
    ] {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("mkdir {dir}: {e}"));
        chown(Path::new(dir), &format!("{owner_uid}:{owner_gid}"));
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
            .unwrap_or_else(|e| panic!("chmod {dir}: {e}"));
    }

    // PR-2 additions (ADR-0019 §1/§5, design doc §7 "reset_state_dirs
    // extended to also remove TRUSTED_CONFIG_PATH and recreate
    // /run/libra-governor"). Every test starts with the trusted config
    // absent — `TrustedConfigGuard` installs it only for the duration of
    // the one test that needs it (design doc §5 "Consequence for the
    // harness").
    let _ = std::fs::remove_file(TRUSTED_CONFIG_PATH);
    let _ = std::fs::remove_dir_all(PROVIDER_SOCKET_DIR);
    std::fs::create_dir_all(PROVIDER_SOCKET_DIR)
        .unwrap_or_else(|e| panic!("mkdir {PROVIDER_SOCKET_DIR}: {e}"));
    chown(
        Path::new(PROVIDER_SOCKET_DIR),
        &format!("{}:{LG_CLIENTS_GID}", LG_PROVIDER.uid),
    );
    // 2710: owner rwx, group --x (traverse only, no list/create/unlink),
    // other none. The setgid bit (2000) makes a socket created inside
    // this directory inherit the lg-clients group.
    std::fs::set_permissions(PROVIDER_SOCKET_DIR, std::fs::Permissions::from_mode(0o2710))
        .unwrap_or_else(|e| panic!("chmod {PROVIDER_SOCKET_DIR}: {e}"));
}

/// Root-owned pinned trusted-principal config path (ADR-0019 §1, mirrors
/// `libra_governor_daemon::trusted_principal::PINNED_CONFIG_PATH` without
/// this crate taking a Cargo dependency on `daemon` — see this crate's
/// `Cargo.toml` doc comment).
pub const TRUSTED_CONFIG_PATH: &str = "/etc/libra-governor/trusted-principal.json";
/// Provider-owned, `lg-clients`-group socket directory (ADR-0019 §1
/// filesystem layout table).
pub const PROVIDER_SOCKET_DIR: &str = "/run/libra-governor";
/// `PROVIDER_SOCKET_DIR`/`daemon.sock`.
pub const PROVIDER_SOCKET: &str = "/run/libra-governor/daemon.sock";
/// Matches `provision.sh`'s `groupadd --gid 11000 lg-clients`.
pub const LG_CLIENTS_GID: u32 = 11_000;

/// The trusted-principal JSON config shape this crate writes, mirroring
/// `libra_governor_daemon::trusted_principal::TrustedPrincipalConfig`'s
/// fields without a Cargo dependency on `daemon` (this crate drives the
/// binary only through `std::process::Command` and stdin/file JSON — see
/// `Cargo.toml`'s doc comment).
#[derive(Debug, Clone)]
pub struct ConfigSpec {
    pub provider_uid: u32,
    pub agent_uids: Vec<u32>,
    pub provider_id: String,
    pub state_dir: String,
    pub socket_dir: String,
    pub connection_timeout_ms: u64,
}

impl Default for ConfigSpec {
    /// The layout every passing (non-adversarial) test uses: the real
    /// provider UID, the real agent UID, and the real state/socket
    /// directories.
    fn default() -> Self {
        Self {
            provider_uid: LG_PROVIDER.uid,
            agent_uids: vec![LG_AGENT.uid],
            provider_id: "operator:lg-provider".to_string(),
            state_dir: PROVIDER_STATE_DIR.to_string(),
            socket_dir: PROVIDER_SOCKET_DIR.to_string(),
            connection_timeout_ms: 5000,
        }
    }
}

impl ConfigSpec {
    fn to_json(&self) -> String {
        serde_json::json!({
            "provider_uid": self.provider_uid,
            "agent_uids": self.agent_uids,
            "provider_id": self.provider_id,
            "state_dir": self.state_dir,
            "socket_dir": self.socket_dir,
            "connection_timeout_ms": self.connection_timeout_ms,
        })
        .to_string()
    }
}

/// Installs the trusted-principal config at `path`, root-owned (`0644`),
/// for the life of this guard — removed on `Drop` so a test's config
/// never leaks into the next one even on an early `panic!` (ADR-0019
/// §5's "every test starts with it absent").
pub struct TrustedConfigGuard {
    path: PathBuf,
}

impl TrustedConfigGuard {
    pub fn install(path: &str, spec: &ConfigSpec) -> Self {
        assert_disposable();
        std::fs::write(path, spec.to_json()).unwrap_or_else(|e| panic!("write {path}: {e}"));
        chown(Path::new(path), "0:0");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
            .unwrap_or_else(|e| panic!("chmod {path}: {e}"));
        Self {
            path: PathBuf::from(path),
        }
    }

    /// Same as [`Self::install`], but root-owned with a caller-chosen
    /// mode and/or owner — for the agent-writable-config refusal cases
    /// (`x_startup_refuses_group_writable_config`, `t08b`'s
    /// agent-written-config case).
    pub fn install_with(path: &str, spec: &ConfigSpec, owner: &str, mode: u32) -> Self {
        assert_disposable();
        std::fs::write(path, spec.to_json()).unwrap_or_else(|e| panic!("write {path}: {e}"));
        chown(Path::new(path), owner);
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .unwrap_or_else(|e| panic!("chmod {path}: {e}"));
        Self {
            path: PathBuf::from(path),
        }
    }
}

impl Drop for TrustedConfigGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

const PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// A running trusted-mode `libra-governor daemon run
/// --trusted-principal-config <path>` process, started as [`LG_PROVIDER`]
/// (trusted mode refuses to start as anyone else — see
/// `check_process_identity`). Readiness is `connect()` succeeding on
/// [`PROVIDER_SOCKET`]; root's own connection is dropped unclassified by
/// the daemon, which is fine for a readiness probe (the connection
/// attempt alone proves the listener is up).
pub struct TrustedDaemon {
    child: Child,
    pub pid: u32,
}

impl TrustedDaemon {
    pub fn start(config_path: &str, envs: &[(&str, &str)], timeout: Duration) -> Self {
        assert_disposable();
        let mut all_envs = vec![("PATH", PATH_ENV)];
        all_envs.extend_from_slice(envs);
        let mut cmd = command_as(
            &LG_PROVIDER,
            PROVIDER_BINARY,
            &["daemon", "run", "--trusted-principal-config", config_path],
            &all_envs,
        );
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to start trusted daemon: {e}"));
        let pid = child.id();

        let deadline = Instant::now() + timeout;
        loop {
            if UnixStream::connect(PROVIDER_SOCKET).is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                panic!(
                    "trusted daemon (pid {pid}) never accepted a connection at \
                     {PROVIDER_SOCKET} within {timeout:?}"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        Self { child, pid }
    }

    /// Starts trusted mode expecting it to **refuse** — as `principal`
    /// (not necessarily [`LG_PROVIDER`]; `t08b` starts this as
    /// [`LG_AGENT`] to prove the UID check alone refuses it), runs to
    /// completion (a refusing startup exits promptly rather than
    /// serving), and asserts both that the exit was non-zero and that no
    /// socket ever came up at [`PROVIDER_SOCKET`]. Returns the process
    /// [`Output`] so the caller can additionally assert on the specific
    /// `StartupError` text in stderr.
    ///
    /// Asserting "not connectable" rather than "the path does not exist"
    /// deliberately tolerates a stale socket file left behind by an
    /// earlier passing start that was killed — a live listener is what
    /// actually matters here, and `connect()` fails the same way
    /// (`ECONNREFUSED`) against a stale file as it does against no file
    /// at all.
    pub fn start_expecting_refusal(
        principal: &Principal,
        config_path: &str,
        envs: &[(&str, &str)],
    ) -> Output {
        assert_disposable();
        let mut all_envs = vec![("PATH", PATH_ENV)];
        all_envs.extend_from_slice(envs);
        let output = run_as(
            principal,
            PROVIDER_BINARY,
            &["daemon", "run", "--trusted-principal-config", config_path],
            &all_envs,
        );
        assert!(
            !output.status.success(),
            "expected trusted-mode startup to be refused (non-zero exit); stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            UnixStream::connect(PROVIDER_SOCKET).is_err(),
            "expected no live listener at {PROVIDER_SOCKET} after a refused trusted-mode start"
        );
        output
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn kill9_and_reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for TrustedDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// This crate takes no Cargo dependency on `libra-governor-protocol`
/// (see `Cargo.toml`'s doc comment), so this is a hand-kept mirror of
/// `libra_governor_protocol::PROTOCOL_VERSION` — bump it if that
/// constant ever changes, or every raw-socket request this crate sends
/// starts failing on a protocol-version mismatch.
const RAW_PROTOCOL_VERSION: u32 = 14;

/// PR-2a/2b's positive control (design doc §7 `trusted_positive_control`):
/// an agent `Preflight` creates a task in the provider ledger over the
/// real trusted socket, then a provider-UID `RecordOutcome` lands a
/// (non-authoritative — authoritative attestation is PR-3) row for it.
/// Both halves go through `lg-raw-client` because no CLI client knows the
/// pinned endpoint before PR-2c.
///
/// `raw_client_bin` is the caller's own
/// `env!("CARGO_BIN_EXE_lg-raw-client")` — resolved in the calling
/// integration test file, not here: `CARGO_BIN_EXE_<name>` is only
/// defined at compile time for an actual integration-test target, and
/// this library crate itself (built as a plain `lib`, not a test binary)
/// has no such variable to read.
pub fn trusted_positive_control(daemon: &TrustedDaemon, raw_client_bin: &str) -> String {
    let _ = daemon;
    let preflight = serde_json::json!({
        "protocol_version": RAW_PROTOCOL_VERSION,
        "request": {
            "kind": "preflight",
            "task_hint": "trusted harness positive control preflight",
            "cwd": "",
            "session_id": "trusted-harness-positive-control",
        }
    })
    .to_string();

    let out = run_as(
        &LG_AGENT,
        raw_client_bin,
        &[PROVIDER_SOCKET, &preflight],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    if !out.status.success() {
        panic!(
            "trusted positive control: preflight via lg-raw-client failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let task_id = query_live(
        PROVIDER_STATE_DIR,
        "SELECT task_id FROM tasks ORDER BY created_at DESC LIMIT 1;",
    )
    .into_iter()
    .next()
    .unwrap_or_else(|| panic!("trusted positive control: no task row appeared after preflight"));

    let record_outcome = serde_json::json!({
        "protocol_version": RAW_PROTOCOL_VERSION,
        "request": {
            "kind": "record_outcome",
            "task_id": task_id,
            "plan_id": null,
            "source_id": "two-principal-harness-trusted-positive-control",
            "idempotency_key": "trusted-harness-positive-control-1",
            "outcome": {"kind": "completed", "evidence": ["harness://trusted-positive-control"]},
            "signed_claim": null,
        }
    })
    .to_string();

    let out = run_as(
        &LG_PROVIDER,
        raw_client_bin,
        &[PROVIDER_SOCKET, &record_outcome],
        &[("PATH", PATH_ENV), ("HOME", "/root")],
    );
    if !out.status.success() {
        panic!(
            "trusted positive control: record_outcome via lg-raw-client failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    task_id
}

use std::os::unix::fs::PermissionsExt;

// Deliberately never interpolates the owner spec (`uid:gid`) into a
// panic message, so no OS-identity-shaped value reaches a `panic!`/
// `format!` sink for CodeQL's `rust/cleartext-logging` heuristic to
// flag. These values are this crate's own hardcoded test constants
// (LG_PROVIDER/LG_AGENT/LG_SUDOPROBE, 10001-10003, defined above in this
// same file) — public OS-level identifiers in a disposable test
// container, never sensitive data — but Rust does not yet support this
// repo's usual inline suppression-comment mechanism, and the path alone
// is sufficient to diagnose a failure here.
fn chown(path: &Path, owner: &str) {
    let status = Command::new("chown")
        .arg(owner)
        .arg(path)
        .status()
        .unwrap_or_else(|_| panic!("chown {path:?}: failed to spawn"));
    if !status.success() {
        panic!("chown {path:?} failed");
    }
}

/// PR-1's positive control (ADR-0019 §6 PR-1: "today's binary runs as
/// `lg-provider`... PR-1's positive control is therefore a task and
/// outcome created by a provider-UID client, not an agent `Preflight`").
/// Starts the daemon as `principal` pinned to `state_dir`, runs
/// `hook user-prompt-submit` as the same principal to create a task,
/// then `outcome record` for that task. Panics (not merely asserts) if
/// either step fails, since every threat test's "the legitimate flow
/// still works" premise depends on this succeeding.
///
/// Returns the running [`DaemonProcess`] (callers decide whether to
/// stop or kill it next) and the `task_id` the daemon assigned.
pub fn positive_control(
    principal: &Principal,
    state_dir: &str,
    home: &str,
) -> (DaemonProcess, String) {
    let daemon = DaemonProcess::start(principal, state_dir, home, Duration::from_secs(10));

    let preflight_payload = serde_json::json!({
        "session_id": "harness-positive-control",
        "cwd": state_dir,
        "prompt": "harness positive control preflight",
    })
    .to_string();

    let out = run_as_with_stdin(
        principal,
        PROVIDER_BINARY,
        &["hook", "user-prompt-submit"],
        &[
            (
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            ),
            ("HOME", home),
            ("LIBRA_GOVERNOR_STATE_DIR", state_dir),
        ],
        &preflight_payload,
    );
    if !out.status.success() {
        panic!(
            "positive control: hook user-prompt-submit failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let task_id = query_live(
        state_dir,
        "SELECT task_id FROM tasks ORDER BY created_at DESC LIMIT 1;",
    )
    .into_iter()
    .next()
    .unwrap_or_else(|| {
        panic!("positive control: no task row appeared in {state_dir} after preflight")
    });

    let outcome_payload = serde_json::json!({
        "task_id": task_id,
        "source_id": "two-principal-harness-positive-control",
        "idempotency_key": "harness-positive-control-1",
        "outcome": {"kind": "completed", "evidence": ["harness://positive-control"]},
    })
    .to_string();

    let out = run_as_with_stdin(
        principal,
        PROVIDER_BINARY,
        &["outcome", "record"],
        &[
            (
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            ),
            ("HOME", home),
            ("LIBRA_GOVERNOR_STATE_DIR", state_dir),
        ],
        &outcome_payload,
    );
    if !out.status.success() {
        panic!(
            "positive control: outcome record failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    (daemon, task_id)
}

/// Reads the daemon's pid-record file (`daemon.pid` inside the state
/// dir), as root — used to compare against what `lg-agent` can read.
pub fn read_pidfile_as_root(state_dir: &str) -> Option<String> {
    std::fs::read_to_string(Path::new(state_dir).join("daemon.pid")).ok()
}

/// Queries the **live** ledger file directly (no scratch copy), as root,
/// with `sqlite3 -readonly`. Only for setup convenience (e.g. "what
/// `task_id` did the positive control just create") — never for the
/// authoritative before/after comparison a threat test asserts on,
/// which must use [`LedgerSnapshot::capture`] instead so a concurrent
/// WAL write can never make two reads of "the same" state disagree.
/// Retries briefly: the positive control's `Preflight` and this query
/// can race the daemon's own commit.
pub fn query_live(state_dir: &str, sql: &str) -> Vec<String> {
    let db = Path::new(state_dir).join("ledger.sqlite3");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if db.exists() {
            let rows = sqlite3_query(&db, sql);
            if !rows.is_empty() {
                return rows;
            }
        }
        if Instant::now() >= deadline {
            return Vec::new();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
