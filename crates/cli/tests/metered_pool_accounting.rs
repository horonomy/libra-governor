//! HORO-2185: consumer-side shared-pool accounting, proven through the real
//! `libra-governor metered-pool` CLI, a real auto-spawned daemon, and (for
//! the SQLite-level race) real separate OS processes opening one ledger
//! file. Evidence class: **TestOwnedLive** — every pool here is provisioned
//! and fed by the test-owned `metered_pool_fixture::SyntheticMeteredFeed`.
//! Nothing here is `ProductionObserved`, and nothing feeds the pool from
//! `quota-observation` (HORO-1915; guarded by
//! `crates/daemon/tests/metered_pool_no_production_feed.rs`).
//!
//! # Identity model actually available today
//!
//! The metered pool knows exactly one consumer identity: the opaque
//! `session_id` string (which must have been established through the real
//! `hook user-prompt-submit` path), under the pool owner's principal.
//! There is NO agent / sub-agent / executor identity, NO parent-child
//! relation, and NO "unknown" bucket anywhere in the ledger, protocol or
//! CLI. This file therefore treats "session / agent / sub-agent" as
//! distinct opaque session ids (it invents no per-product ids) and proves
//! what exists. What is absent is listed in the PR body (Host Integration).
//!
//! # Parent/child non-duplication rule (asserted at the level that exists)
//!
//! Spend is recorded exactly once, at the narrowest known scope (the
//! settling session). A session's `settled` is its SELF total and never
//! includes another session's spend. A subtree total, were it ever
//! reported, would equal self + children's selves; no such API exists, so
//! the test computes it as arithmetic and asserts the invariant that makes
//! it well-defined: pool settled == sum of per-session self totals.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use time::OffsetDateTime;

use libra_governor_domain::{PoolId, PrincipalId, QuotaUnit};
use libra_governor_ledger::{
    LedgerStore, MeteredReserveOutcome, MeteredReserveRequest, SharedPoolReserveRequest,
};

// This suite only uses the fixture's `publish_used`; the stale/undisclosed
// publishers are exercised by the golden journey.
#[allow(dead_code)]
mod metered_pool_fixture;
mod support;

use metered_pool_fixture::SyntheticMeteredFeed;
use serde_json::Value;

const OWNER: &str = "acme";
const ALIAS: &str = "usd-2026-10";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

struct Sandbox {
    state: support::DaemonState,
    home: tempfile::TempDir,
    /// The cwd every `establish_session` call uses, so a test can later
    /// run a CLI process from that same cwd to prove identity is never
    /// guessed from it.
    repo: tempfile::TempDir,
}

impl Sandbox {
    /// A sandbox with a provisioned, freshly fed pool of `capacity` cents
    /// and the given sessions already established.
    fn with_pool(capacity: u64, sessions: &[&str]) -> (Self, SyntheticMeteredFeed) {
        let sandbox = Self {
            state: support::DaemonState::new(),
            home: tempfile::tempdir().unwrap(),
            repo: tempfile::tempdir().unwrap(),
        };
        for s in sessions {
            sandbox.establish_session(s);
        }
        let feed = SyntheticMeteredFeed::provision(
            sandbox.state.path(),
            OWNER,
            ALIAS,
            QuotaUnit::UsdCents,
            capacity,
        );
        feed.publish_used(0, Some(capacity), OffsetDateTime::now_utc());
        (sandbox, feed)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(bin());
        cmd.args(args)
            .env("LIBRA_GOVERNOR_STATE_DIR", self.state.path())
            .env("HOME", self.home.path());
        cmd
    }

    fn establish_session(&self, session_id: &str) {
        use std::io::Write;
        let payload = serde_json::json!({
            "session_id": session_id,
            "cwd": self.repo.path(),
            "prompt": "do work",
            "hook_event_name": "UserPromptSubmit",
            "transcript_path": "/tmp/does-not-matter.jsonl",
        })
        .to_string();
        let mut child = self
            .command(&["hook", "user-prompt-submit"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn hook user-prompt-submit");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "establish_session failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run(&self, args: &[&str]) -> (i32, Value) {
        parse_output(self.command(args).output().expect("spawn libra-governor"))
    }

    fn reserve(&self, session: &str, amount: u64, key: &str) -> (i32, Value) {
        self.run_owned(&reserve_args(session, amount, key))
    }

    fn settle(&self, id: &str, session: &str, actual: Option<u64>) -> (i32, Value) {
        let actual = actual.map(|a| a.to_string());
        let mut args = vec![
            "metered-pool",
            "settle",
            "--reservation",
            id,
            "--owner",
            OWNER,
            "--session",
            session,
        ];
        if let Some(a) = actual.as_deref() {
            args.extend(["--actual", a]);
        }
        self.run(&args)
    }

    fn release(&self, id: &str, session: &str) -> (i32, Value) {
        self.run(&[
            "metered-pool",
            "release",
            "--reservation",
            id,
            "--owner",
            OWNER,
            "--session",
            session,
        ])
    }

    fn status(&self) -> Value {
        let (code, v) = self.run(&["metered-pool", "status", "--owner", OWNER, "--pool", ALIAS]);
        assert_eq!(code, 0, "status must succeed: {v}");
        v
    }

    fn ledger_path(&self) -> PathBuf {
        self.state.path().join("ledger.sqlite3")
    }
}

fn reserve_args(session: &str, amount: u64, key: &str) -> Vec<String> {
    [
        "metered-pool",
        "reserve",
        "--owner",
        OWNER,
        "--pool",
        ALIAS,
        "--unit",
        "usd_cents",
        "--amount",
        &amount.to_string(),
        "--session",
        session,
        "--key",
        key,
    ]
    .map(String::from)
    .to_vec()
}

impl Sandbox {
    fn run_owned(&self, args: &[String]) -> (i32, Value) {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&refs)
    }
}

fn parse_output(output: std::process::Output) -> (i32, Value) {
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "no JSON on stdout (exit {code}): {e}\nstdout: {stdout:?}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (code, value)
}

fn id_of(v: &Value) -> String {
    v["reservation_id"]
        .as_str()
        .unwrap_or_else(|| panic!("expected reservation_id in {v}"))
        .to_string()
}

/// `(active, settled)` for `session` from a status report, or `None` if
/// the session has no row.
fn usage(status: &Value, session: &str) -> Option<(u64, u64)> {
    status["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == session)
        .map(|s| {
            (
                s["active"].as_u64().unwrap(),
                s["settled"].as_u64().unwrap(),
            )
        })
}

fn sum_sessions(status: &Value, field: &str) -> u64 {
    status["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s[field].as_u64().unwrap())
        .sum()
}

// ---------------------------------------------------------------------
// 1. Different identities, ONE pool.
// ---------------------------------------------------------------------

#[test]
fn sessions_agents_and_subagents_all_spend_from_one_pool() {
    // Opaque, distinct session ids standing in for a lead session, an
    // agent and a sub-agent. They are plain strings: the pool has no
    // notion of which is which.
    let ids = ["lead", "agent-a", "subagent-a1"];
    let (sb, _feed) = Sandbox::with_pool(10_000, &ids);

    let mut pool_ids = Vec::new();
    let mut spent = 0;
    for (i, s) in ids.iter().enumerate() {
        let amount = 1_000 * (i as u64 + 1);
        let (c, r) = sb.reserve(s, amount, "k");
        assert_eq!(c, 0, "{r}");
        pool_ids.push(r["pool_id"].as_str().unwrap().to_string());
        let (c, st) = sb.settle(&id_of(&r), s, Some(amount));
        assert_eq!(c, 0, "{st}");
        spent += amount;
        // Shared capacity visibly decreases with each identity's spend.
        let status = sb.status();
        assert_eq!(status["settled"], spent);
        assert_eq!(status["remaining"], 10_000 - spent as i64);
    }
    assert!(pool_ids.windows(2).all(|w| w[0] == w[1]), "one pool id");
    let status = sb.status();
    assert_eq!(status["sessions"].as_array().unwrap().len(), 3);
    assert_eq!(status["remaining"], 4_000);

    // The last identity can be refused purely because of what OTHER
    // identities already consumed: capacity is shared, not per-session.
    let (c, over) = sb.reserve("subagent-a1", 4_001, "big");
    assert_eq!(c, 3, "{over}");
    assert_eq!(over["reason"]["reason"], "insufficient");
    assert_eq!(over["reason"]["remaining"], 4_000);
}

// ---------------------------------------------------------------------
// 3. Attribution.
// ---------------------------------------------------------------------

#[test]
fn settled_spend_is_reported_under_the_settling_session() {
    let (sb, _feed) = Sandbox::with_pool(50_000, &["exec-a", "exec-b"]);
    let (_, a) = sb.reserve("exec-a", 9_000, "a1");
    let (_, b) = sb.reserve("exec-b", 9_000, "b1");
    let (c, _) = sb.settle(&id_of(&a), "exec-a", Some(700));
    assert_eq!(c, 0);
    let (c, _) = sb.settle(&id_of(&b), "exec-b", Some(300));
    assert_eq!(c, 0);

    let status = sb.status();
    assert_eq!(usage(&status, "exec-a"), Some((0, 700)));
    assert_eq!(usage(&status, "exec-b"), Some((0, 300)));

    // A session cannot move spend onto itself by settling another
    // session's hold: refused, attribution unchanged.
    let (_, a2) = sb.reserve("exec-a", 1_000, "a2");
    let (c, refused) = sb.settle(&id_of(&a2), "exec-b", Some(1_000));
    assert_eq!(c, 3, "{refused}");
    assert_eq!(refused["reason"]["reason"], "not_holder");
    let status = sb.status();
    assert_eq!(usage(&status, "exec-a"), Some((1_000, 700)));
    assert_eq!(usage(&status, "exec-b"), Some((0, 300)));
}

// ---------------------------------------------------------------------
// 4 & 6. Missing / unknown identity.
// ---------------------------------------------------------------------

#[test]
fn identity_less_reserve_and_settle_are_refused_and_never_charged() {
    let (sb, _feed) = Sandbox::with_pool(10_000, &["known"]);
    let (_, held) = sb.reserve("known", 2_000, "k1");
    let before = sb.status();
    assert_eq!(before["active"], 2_000);

    // Empty session id: refused as reserved scope.
    let (c, r) = sb.reserve("", 500, "e1");
    assert_eq!(c, 3, "{r}");
    assert_eq!(r["reason"]["reason"], "reserved_session_scope");

    // Missing flag entirely: usage error (exit 2), nothing sent.
    let args: Vec<String> = reserve_args("x", 500, "e2")
        .into_iter()
        .filter(|a| a != "--session" && a != "x")
        .collect();
    let out = sb
        .command(&args.iter().map(String::as_str).collect::<Vec<_>>())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());

    // Never-established session: refused, no hold.
    let (c, r) = sb.reserve("never-established", 500, "e3");
    assert_eq!(c, 3, "{r}");
    assert_eq!(r["reason"]["reason"], "unknown_session");

    // Identity-less settle of a real hold: refused, hold untouched.
    let (c, r) = sb.settle(&id_of(&held), "", Some(2_000));
    assert_eq!(c, 3, "{r}");
    assert_eq!(r["reason"]["reason"], "not_holder");

    // Neither dropped silently (explicit refusals above) nor charged
    // anywhere: pool and per-session totals are exactly as before, and no
    // synthetic bucket for the missing identity appeared.
    let after = sb.status();
    assert_eq!(after["active"], 2_000);
    assert_eq!(after["settled"], 0);
    assert_eq!(after["sessions"], before["sessions"]);
    assert_eq!(after["sessions"].as_array().unwrap().len(), 1);
}

#[test]
fn unknown_identity_is_never_guessed_from_cwd_pid_or_tmux() {
    let (sb, _feed) = Sandbox::with_pool(10_000, &["real-session"]);
    // Run the CLI from the very cwd `real-session` was established in,
    // with tmux-looking env, under a session id the daemon never saw.
    let mut cmd = sb.command(
        &reserve_args("ghost", 1_000, "g1")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    cmd.current_dir(sb.repo.path())
        .env("TMUX", "/tmp/tmux-501/default,1234,0")
        .env("TMUX_PANE", "%1");
    let (c, r) = parse_output(cmd.output().unwrap());
    assert_eq!(c, 3, "{r}");
    assert_eq!(r["reason"]["reason"], "unknown_session");

    let status = sb.status();
    assert_eq!(status["active"], 0);
    assert!(usage(&status, "ghost").is_none());
    assert!(
        usage(&status, "real-session").is_none(),
        "the ghost's spend must not be adopted by the session sharing its cwd"
    );
}

// ---------------------------------------------------------------------
// 5 & 8. Parent/child non-duplication and exact reconciliation.
// ---------------------------------------------------------------------

#[test]
fn parent_and_child_spend_is_recorded_once_and_reconciles_with_the_pool() {
    let (sb, feed) = Sandbox::with_pool(100_000, &["parent", "child-1", "child-2"]);

    // (session, reserved, settled-or-None-for-release, key)
    let plan: [(&str, u64, Option<u64>, &str); 7] = [
        ("parent", 10_000, Some(6_500), "p1"),
        ("parent", 3_000, Some(3_000), "p2"),
        ("child-1", 8_000, Some(2_345), "c1a"),
        ("child-1", 5_000, None, "c1b"), // released unspent
        ("child-2", 7_000, Some(7_000), "c2a"),
        ("child-2", 4_000, Some(1), "c2b"),
        ("child-1", 2_500, Some(2_500), "c1c"),
    ];
    let mut own: std::collections::HashMap<&str, u64> = Default::default();
    for (session, reserved, settle_with, key) in plan {
        let (c, r) = sb.reserve(session, reserved, key);
        assert_eq!(c, 0, "{r}");
        let id = id_of(&r);
        match settle_with {
            Some(actual) => {
                let (c, s) = sb.settle(&id, session, Some(actual));
                assert_eq!(c, 0, "{s}");
                *own.entry(session).or_default() += actual;
                // Replayed settle / replayed reserve never add spend.
                let (c, replay) = sb.settle(&id, session, Some(actual + 99));
                assert_eq!(c, 0, "{replay}");
                assert_eq!(replay["settled_amount"], actual);
                let (c, rr) = sb.reserve(session, reserved, key);
                assert_eq!(c, 0, "{rr}");
            }
            None => {
                let (c, rel) = sb.release(&id, session);
                assert_eq!(c, 0, "{rel}");
            }
        }
    }

    let status = sb.status();
    let parent_self = own["parent"];
    let children_self = own["child-1"] + own["child-2"];
    // Self totals: parent's self excludes everything its children spent.
    assert_eq!(usage(&status, "parent"), Some((0, parent_self)));
    assert_eq!(parent_self, 9_500);
    assert_eq!(usage(&status, "child-1"), Some((0, own["child-1"])));
    assert_eq!(usage(&status, "child-2"), Some((0, own["child-2"])));
    // Subtree (computed here; no API) is distinct from self.
    let parent_subtree = parent_self + children_self;
    assert_ne!(parent_subtree, parent_self);
    // Recorded once: pool total == sum of self totals (no unknown bucket
    // exists, so that term is identically zero) == subtree of the root.
    assert_eq!(
        sum_sessions(&status, "settled"),
        status["settled"].as_u64().unwrap()
    );
    assert_eq!(status["settled"].as_u64().unwrap(), parent_subtree);
    assert_eq!(parent_subtree, 9_500 + 2_345 + 2_500 + 7_000 + 1);
    assert_eq!(status["active"], 0);
    assert_eq!(sum_sessions(&status, "active"), 0);
    assert_eq!(status["remaining"], 100_000 - parent_subtree as i64);

    // Reconciliation survives a provider gauge equal to Libra's own
    // settled total (no double counting) ...
    feed.publish_used(parent_subtree, Some(100_000), OffsetDateTime::now_utc());
    let status = sb.status();
    assert_eq!(status["remaining"], 100_000 - parent_subtree as i64);
    assert_eq!(
        sum_sessions(&status, "settled"),
        status["settled"].as_u64().unwrap()
    );
}

// ---------------------------------------------------------------------
// 2a. Multi-process race through the real CLI + daemon.
// ---------------------------------------------------------------------

/// Spawns `args` as a real process that signals readiness by creating
/// `ready` and then spins until `gate` exists, then `exec`s the binary —
/// so every contender's CLI start is released by one file creation.
fn spawn_gated(sb: &Sandbox, ready: &Path, gate: &Path, args: &[String]) -> std::process::Child {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(r#"touch "$READY"; while [ ! -e "$GATE" ]; do :; done; exec "$@""#)
        .arg("sh")
        .arg(bin())
        .args(args)
        .env("READY", ready)
        .env("GATE", gate)
        .env("LIBRA_GOVERNOR_STATE_DIR", sb.state.path())
        .env("HOME", sb.home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn().expect("spawn gated contender")
}

fn wait_all_exist(paths: &[PathBuf]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !paths.iter().all(|p| p.exists()) {
        assert!(Instant::now() < deadline, "contenders never became ready");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn multi_process_cli_reserve_race_never_overallocates() {
    const N: usize = 16;
    const AMOUNT: u64 = 4_000;
    const CAPACITY: u64 = 50_000;
    let sessions: Vec<String> = (0..N).map(|i| format!("racer-{i}")).collect();
    let refs: Vec<&str> = sessions.iter().map(String::as_str).collect();
    let (sb, _feed) = Sandbox::with_pool(CAPACITY, &refs);

    let gate_dir = tempfile::tempdir().unwrap();
    let gate = gate_dir.path().join("go");
    let readies: Vec<PathBuf> = (0..N)
        .map(|i| gate_dir.path().join(format!("r{i}")))
        .collect();
    let children: Vec<_> = (0..N)
        .map(|i| {
            spawn_gated(
                &sb,
                &readies[i],
                &gate,
                &reserve_args(&sessions[i], AMOUNT, "k"),
            )
        })
        .collect();
    wait_all_exist(&readies);
    std::fs::write(&gate, b"").unwrap();

    let (mut granted, mut refused, mut granted_sum) = (0u64, 0u64, 0u64);
    for child in children {
        let (code, v) = parse_output(child.wait_with_output().unwrap());
        match code {
            0 => {
                granted += 1;
                granted_sum += v["amount"].as_u64().unwrap();
            }
            3 => {
                assert_eq!(v["reason"]["reason"], "insufficient", "{v}");
                refused += 1;
            }
            other => panic!("unexpected exit {other}: {v}"),
        }
    }
    assert_eq!(granted + refused, N as u64, "every contender got an answer");
    assert!(granted_sum <= CAPACITY, "over-allocated: {granted_sum}");
    assert_eq!(granted, CAPACITY / AMOUNT, "exactly floor(capacity/amount)");
    let status = sb.status();
    assert_eq!(status["active"].as_u64().unwrap(), granted_sum);
    assert_eq!(sum_sessions(&status, "active"), granted_sum);
    assert_eq!(status["remaining"], CAPACITY as i64 - granted_sum as i64);
}

// ---------------------------------------------------------------------
// 2b. Multi-process race at the SQLite level (separate LedgerStore
//     processes on ONE file, re-exec of this very test binary).
// ---------------------------------------------------------------------

const WORKER_ENV: &str = "HORO2185_WORKER";
const WORKER_RESERVES: usize = 3;
const WORKER_AMOUNT: u64 = 3_000;

/// Not a real test: the body of one racing process. A no-op unless the
/// parent test re-executes this binary with `HORO2185_WORKER` set.
#[test]
fn ledger_race_worker_entrypoint() {
    let Ok(spec) = std::env::var(WORKER_ENV) else {
        return;
    };
    // spec = "<db>|<ready>|<gate>|<worker-index>"
    let parts: Vec<&str> = spec.split('|').collect();
    let (db, ready, gate, idx) = (parts[0], parts[1], parts[2], parts[3]);
    let mut store = LedgerStore::open(Path::new(db)).unwrap();
    std::fs::write(ready, b"").unwrap();
    while !Path::new(gate).exists() {
        std::hint::spin_loop();
    }
    let pool_id = PoolId(format!("metered:{OWNER}/{ALIAS}"));
    let principal = PrincipalId(OWNER.to_string());
    let session = format!("proc-{idx}");
    for n in 0..WORKER_RESERVES {
        let key = format!("{idx}-{n}");
        let outcome = store
            .reserve_shared_metered(MeteredReserveRequest {
                base: SharedPoolReserveRequest {
                    pool_id: &pool_id,
                    principal_id: &principal,
                    session_id: &session,
                    amount: WORKER_AMOUNT,
                    unit: &QuotaUnit::UsdCents,
                    idempotency_key: &key,
                    now: OffsetDateTime::now_utc(),
                    ttl_secs: 3600,
                },
                session_cap: None,
            })
            .unwrap();
        let tag = match outcome {
            MeteredReserveOutcome::Granted(r) => format!("GRANTED {}", r.amount),
            MeteredReserveOutcome::Insufficient { .. } => "REFUSED".to_string(),
            other => panic!("unexpected outcome {other:?}"),
        };
        println!("HORO2185_RESULT {tag}");
    }
}

#[test]
fn multi_process_ledger_store_race_on_one_sqlite_file_never_overallocates() {
    const PROCS: usize = 8;
    const CAPACITY: u64 = 50_000;
    let sb_state = support::DaemonState::new();
    let feed = SyntheticMeteredFeed::provision(
        sb_state.path(),
        OWNER,
        ALIAS,
        QuotaUnit::UsdCents,
        CAPACITY,
    );
    feed.publish_used(0, Some(CAPACITY), OffsetDateTime::now_utc());
    let db = sb_state.path().join("ledger.sqlite3");

    let dir = tempfile::tempdir().unwrap();
    let gate = dir.path().join("go");
    let readies: Vec<PathBuf> = (0..PROCS)
        .map(|i| dir.path().join(format!("r{i}")))
        .collect();
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..PROCS)
        .map(|i| {
            Command::new(&exe)
                .args([
                    "--exact",
                    "ledger_race_worker_entrypoint",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(
                    WORKER_ENV,
                    format!(
                        "{}|{}|{}|{i}",
                        db.display(),
                        readies[i].display(),
                        gate.display()
                    ),
                )
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    wait_all_exist(&readies);
    std::fs::write(&gate, b"").unwrap();

    let (mut granted, mut refused, mut granted_sum) = (0u64, 0u64, 0u64);
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "worker failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        for line in BufReader::new(&out.stdout[..]).lines() {
            let line = line.unwrap();
            // libtest prefixes `test <name> ... ` to the first line of
            // captured-off output, so match the marker anywhere.
            let marker = "HORO2185_RESULT ";
            let Some(pos) = line.find(marker) else {
                continue;
            };
            let tag = &line[pos + marker.len()..];
            if let Some(a) = tag.strip_prefix("GRANTED ") {
                granted += 1;
                granted_sum += a.parse::<u64>().unwrap();
            } else {
                assert_eq!(tag, "REFUSED");
                refused += 1;
            }
        }
    }
    let attempts = (PROCS * WORKER_RESERVES) as u64;
    assert_eq!(
        granted + refused,
        attempts,
        "every attempt answered exactly once"
    );
    assert!(granted_sum <= CAPACITY, "over-allocated: {granted_sum}");
    assert_eq!(granted, CAPACITY / WORKER_AMOUNT);
    assert_eq!(granted_sum, granted * WORKER_AMOUNT);

    let store = LedgerStore::open(&db).unwrap();
    let pool_id = PoolId(format!("metered:{OWNER}/{ALIAS}"));
    let held: u64 = store
        .shared_pool_session_usage(&pool_id)
        .unwrap()
        .iter()
        .map(|u| u.active)
        .sum();
    assert_eq!(
        held, granted_sum,
        "ledger rows agree with what processes were told"
    );
}

// ---------------------------------------------------------------------
// 7. Crash / restart.
// ---------------------------------------------------------------------

fn sigkill_daemon(sb: &Sandbox) {
    let record: Value =
        serde_json::from_slice(&std::fs::read(sb.state.path().join("daemon.pid")).unwrap())
            .unwrap();
    let pid = rustix::process::Pid::from_raw(record["pid"].as_u64().unwrap() as i32).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
    // Wait until the process is really gone so the next CLI call must
    // spawn a fresh daemon rather than racing the dying one.
    let deadline = Instant::now() + Duration::from_secs(10);
    while rustix::process::test_kill_process(pid).is_ok() {
        assert!(Instant::now() < deadline, "daemon did not die");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn sigkilled_daemon_keeps_settled_totals_and_holds_with_no_phantom_capacity() {
    let (sb, _feed) = Sandbox::with_pool(20_000, &["s1", "s2"]);
    let (_, a) = sb.reserve("s1", 6_000, "a");
    let (_, b) = sb.reserve("s2", 5_000, "b");
    let (c, _) = sb.settle(&id_of(&a), "s1", Some(4_000));
    assert_eq!(c, 0);
    let before = sb.status();

    sigkill_daemon(&sb);

    // A fresh daemon (spawned by the next mutating call) sees identical
    // durable state; the surviving hold still counts against capacity.
    let (c, over) = sb.reserve("s1", 11_001, "too-big");
    assert_eq!(c, 3, "{over}");
    assert_eq!(over["reason"]["reason"], "insufficient");
    assert_eq!(over["reason"]["remaining"], 11_000);
    let after = sb.status();
    assert_eq!(after["settled"], 4_000);
    assert_eq!(after["active"], 5_000);
    assert_eq!(after["sessions"], before["sessions"]);
    assert_eq!(usage(&after, "s1"), Some((0, 4_000)));
    assert_eq!(usage(&after, "s2"), Some((5_000, 0)));
    // The pre-crash hold is still settleable by its owner exactly once.
    let (c, s) = sb.settle(&id_of(&b), "s2", Some(5_000));
    assert_eq!(c, 0, "{s}");
    assert_eq!(sb.status()["settled"], 9_000);
}

#[test]
fn reserve_client_killed_mid_flight_never_double_charges_on_retry() {
    const ATTEMPTS: usize = 8;
    const AMOUNT: u64 = 1_000;
    let (sb, _feed) = Sandbox::with_pool(100_000, &["victim"]);
    // Warm daemon so the kill lands on the client, not daemon startup.
    sb.status_or_spawn_for_warmup();

    for i in 0..ATTEMPTS {
        let mut child = sb
            .command(
                &reserve_args("victim", AMOUNT, &format!("k{i}"))
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Vary the kill point; whether the hold landed is intentionally
        // not assumed.
        std::thread::sleep(Duration::from_micros(300 * i as u64));
        let _ = child.kill();
        let _ = child.wait();
    }
    // Retrying every key must converge to exactly one hold per key,
    // whether or not the killed attempt had been committed.
    for i in 0..ATTEMPTS {
        let (c, r) = sb.reserve("victim", AMOUNT, &format!("k{i}"));
        assert_eq!(c, 0, "{r}");
    }
    let status = sb.status();
    assert_eq!(status["active"].as_u64().unwrap(), ATTEMPTS as u64 * AMOUNT);
    assert_eq!(
        usage(&status, "victim"),
        Some((ATTEMPTS as u64 * AMOUNT, 0))
    );
    assert_eq!(
        status["remaining"],
        100_000 - (ATTEMPTS as i64 * AMOUNT as i64)
    );
}

impl Sandbox {
    fn status_or_spawn_for_warmup(&self) {
        // `status` never spawns a daemon; a settle of an unknown id does.
        let (code, _) = self.run(&[
            "metered-pool",
            "release",
            "--reservation",
            "00000000-0000-0000-0000-000000000000",
            "--owner",
            OWNER,
            "--session",
            "victim",
        ]);
        assert_eq!(code, 3);
    }
}

#[test]
fn stale_holds_are_reclaimed_after_daemon_death_and_free_exact_capacity() {
    let (sb, _feed) = Sandbox::with_pool(30_000, &["crashed", "other"]);
    let (c, held) = sb.reserve("crashed", 30_000, "all");
    assert_eq!(c, 0, "{held}");
    // Pool is fully held: nobody else gets anything.
    let (c, r) = sb.reserve("other", 1, "o1");
    assert_eq!(c, 3, "{r}");

    sigkill_daemon(&sb);

    // Within the TTL a sweep reclaims nothing (no premature phantom free
    // capacity); past the TTL it reclaims exactly the dead hold.
    let pool_id = PoolId(format!("metered:{OWNER}/{ALIAS}"));
    let mut store = LedgerStore::open(sb.ledger_path()).unwrap();
    let now = OffsetDateTime::now_utc();
    store
        .expire_stale_shared_pool_reservations(now + time::Duration::minutes(30))
        .unwrap();
    assert_eq!(held_total(&store, &pool_id), 30_000);
    store
        .expire_stale_shared_pool_reservations(now + time::Duration::hours(2))
        .unwrap();
    assert_eq!(held_total(&store, &pool_id), 0);
    drop(store);

    // Real restarted daemon: full capacity is available again, and not
    // one cent more than capacity.
    let (c, r) = sb.reserve("other", 30_000, "o2");
    assert_eq!(c, 0, "{r}");
    let (c, r) = sb.reserve("other", 1, "o3");
    assert_eq!(c, 3, "{r}");
    let status = sb.status();
    assert_eq!(status["active"], 30_000);
    assert_eq!(status["settled"], 0);
}

fn held_total(store: &LedgerStore, pool_id: &PoolId) -> u64 {
    store
        .shared_pool_session_usage(pool_id)
        .unwrap()
        .iter()
        .map(|u| u.active)
        .sum()
}
