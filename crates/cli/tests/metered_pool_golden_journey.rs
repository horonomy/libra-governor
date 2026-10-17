//! End-to-end golden journey for HORO-1915's metered shared pool: the
//! real `libra-governor metered-pool <reserve|settle|release|status>`
//! CLI, against a real auto-spawned daemon, provisioned and fed entirely
//! by the test-owned `metered_pool_fixture::SyntheticMeteredFeed`
//! (synthetic — never the real quota-observation pipeline; see that
//! module's own docs).
//!
//! Proves the one-shared-pool enterprise invariant end to end: two
//! independent sessions consuming one authoritative pool without
//! duplication, no double counting between a provider gauge and Libra's
//! own settlement receipts, no cross-principal contamination, no
//! oversubscription under concurrency, no phantom free capacity after a
//! daemon restart, fail-closed behavior on a stale/unknown snapshot or a
//! unit/owner mismatch, and correct attribution/partial-settlement
//! accounting.

use std::process::{Command, Stdio};
use std::time::Duration;

use time::OffsetDateTime;

mod metered_pool_fixture;
mod support;

use metered_pool_fixture::SyntheticMeteredFeed;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

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

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(bin());
        cmd.args(args)
            .env("LIBRA_GOVERNOR_STATE_DIR", self.state.path())
            .env("HOME", self.home.path());
        cmd
    }

    /// Establishes `session_id` -> a real `TaskId` via the real
    /// `hook user-prompt-submit` path (the same Preflight path every
    /// other integration suite in this crate uses) — a metered reserve
    /// refuses `UnknownSession` for any session that has not gone through
    /// this.
    fn establish_session(&self, session_id: &str) {
        use std::io::Write;
        let repo = tempfile::tempdir().unwrap();
        let payload = serde_json::json!({
            "session_id": session_id,
            "cwd": repo.path(),
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

    fn provision(
        &self,
        owner: &str,
        alias: &str,
        unit: libra_governor_domain::QuotaUnit,
        capacity: u64,
    ) -> SyntheticMeteredFeed {
        SyntheticMeteredFeed::provision(self.state.path(), owner, alias, unit, capacity)
    }

    /// Runs `metered-pool <subcommand+args>` against the real binary and
    /// parses its one JSON-line stdout into a `serde_json::Value`, along
    /// with the process exit code.
    fn metered_pool(&self, args: &[&str]) -> (i32, serde_json::Value) {
        let mut full_args = vec!["metered-pool"];
        full_args.extend_from_slice(args);
        let output = self
            .command(&full_args)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn libra-governor {full_args:?}: {e}"));
        let code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
            panic!(
                "metered-pool {args:?} did not print valid JSON (exit {code}): {e}\nstdout: {stdout:?}\nstderr: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (code, value)
    }

    fn reserve(
        &self,
        owner: &str,
        alias: &str,
        session: &str,
        amount: u64,
        key: &str,
    ) -> (i32, serde_json::Value) {
        self.metered_pool(&[
            "reserve",
            "--owner",
            owner,
            "--pool",
            alias,
            "--unit",
            "usd_cents",
            "--amount",
            &amount.to_string(),
            "--session",
            session,
            "--key",
            key,
        ])
    }

    fn reserve_capped(
        &self,
        owner: &str,
        alias: &str,
        session: &str,
        amount: u64,
        key: &str,
        cap: u64,
    ) -> (i32, serde_json::Value) {
        self.metered_pool(&[
            "reserve",
            "--owner",
            owner,
            "--pool",
            alias,
            "--unit",
            "usd_cents",
            "--amount",
            &amount.to_string(),
            "--session",
            session,
            "--key",
            key,
            "--session-cap",
            &cap.to_string(),
        ])
    }

    fn settle(
        &self,
        reservation_id: &str,
        owner: &str,
        session: &str,
        actual: Option<u64>,
    ) -> (i32, serde_json::Value) {
        let mut args = vec![
            "settle",
            "--reservation",
            reservation_id,
            "--owner",
            owner,
            "--session",
            session,
        ];
        let actual_str;
        if let Some(a) = actual {
            actual_str = a.to_string();
            args.push("--actual");
            args.push(&actual_str);
        }
        self.metered_pool(&args)
    }

    fn release(
        &self,
        reservation_id: &str,
        owner: &str,
        session: &str,
    ) -> (i32, serde_json::Value) {
        self.metered_pool(&[
            "release",
            "--reservation",
            reservation_id,
            "--owner",
            owner,
            "--session",
            session,
        ])
    }

    fn status(&self, owner: &str, alias: &str) -> (i32, serde_json::Value) {
        self.metered_pool(&["status", "--owner", owner, "--pool", alias])
    }
}

fn reservation_id(v: &serde_json::Value) -> String {
    v["reservation_id"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a reservation_id in {v}"))
        .to_string()
}

// ---------------------------------------------------------------------
// 1. Shared, not duplicated.
// ---------------------------------------------------------------------

#[test]
fn shared_pool_is_not_duplicated_per_session() {
    let sandbox = Sandbox::new();
    sandbox.establish_session("sess-a");
    sandbox.establish_session("sess-b");
    let _feed = sandbox.provision(
        "acme",
        "usd-2026-10",
        libra_governor_domain::QuotaUnit::UsdCents,
        50_000,
    );
    {
        let feed = SyntheticMeteredFeed::provision(
            sandbox.state.path(),
            "acme",
            "usd-2026-10",
            libra_governor_domain::QuotaUnit::UsdCents,
            50_000,
        );
        feed.publish_used(0, Some(50_000), OffsetDateTime::now_utc());
    }

    let (code_a, a) = sandbox.reserve("acme", "usd-2026-10", "sess-a", 20_000, "a1");
    assert_eq!(code_a, 0, "{a}");
    let (code_b, b) = sandbox.reserve("acme", "usd-2026-10", "sess-b", 20_000, "b1");
    assert_eq!(code_b, 0, "{b}");
    assert_eq!(
        a["pool_id"], b["pool_id"],
        "both sessions must share one pool id"
    );

    let (_, status) = sandbox.status("acme", "usd-2026-10");
    assert_eq!(status["active"], 40_000);
    let sessions = status["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);

    let (code_over, over) = sandbox.reserve("acme", "usd-2026-10", "sess-a", 15_000, "a2");
    assert_eq!(code_over, 3, "{over}");
    assert_eq!(over["reason"]["reason"], "insufficient");
}

// ---------------------------------------------------------------------
// 2 & 3. Partial settle/release, and no double counting gauge+receipts.
// ---------------------------------------------------------------------

#[test]
fn partial_settle_release_and_gauge_receipts_do_not_double_count() {
    let sandbox = Sandbox::new();
    sandbox.establish_session("sess-a");
    sandbox.establish_session("sess-b");
    let feed = sandbox.provision(
        "acme",
        "usd-2026-10",
        libra_governor_domain::QuotaUnit::UsdCents,
        50_000,
    );
    feed.publish_used(0, Some(50_000), OffsetDateTime::now_utc());

    let (_, a) = sandbox.reserve("acme", "usd-2026-10", "sess-a", 20_000, "a1");
    let (_, b) = sandbox.reserve("acme", "usd-2026-10", "sess-b", 20_000, "b1");
    let a_id = reservation_id(&a);
    let b_id = reservation_id(&b);

    let (code, settled) = sandbox.settle(&a_id, "acme", "sess-a", Some(12_000));
    assert_eq!(code, 0, "{settled}");
    assert_eq!(settled["settled_amount"], 12_000);
    assert_eq!(settled["refunded"], 8_000);

    let (code, released) = sandbox.release(&b_id, "acme", "sess-b");
    assert_eq!(code, 0, "{released}");

    let (_, status) = sandbox.status("acme", "usd-2026-10");
    assert_eq!(status["settled"], 12_000);
    assert_eq!(status["active"], 0);
    assert_eq!(status["remaining"], 38_000);

    // Replays never double-apply.
    let (code, replay_settle) = sandbox.settle(&a_id, "acme", "sess-a", Some(1));
    assert_eq!(code, 0, "{replay_settle}");
    assert_eq!(replay_settle["settled_amount"], 12_000, "{replay_settle}");
    let (code, replay_release) = sandbox.release(&b_id, "acme", "sess-b");
    assert_eq!(code, 0, "{replay_release}");

    // Publishing the exact settled amount as the provider's own gauge
    // must not subtract it a second time.
    feed.publish_used(12_000, Some(50_000), OffsetDateTime::now_utc());
    let (_, status) = sandbox.status("acme", "usd-2026-10");
    assert_eq!(
        status["remaining"], 38_000,
        "settled amount must not be double-counted via the gauge"
    );

    // A reading ABOVE what Libra knows about is genuine external overhang
    // and must reduce remaining further.
    feed.publish_used(17_000, Some(50_000), OffsetDateTime::now_utc());
    let (_, status) = sandbox.status("acme", "usd-2026-10");
    assert_eq!(status["remaining"], 33_000);

    let (code, at_limit) = sandbox.reserve("acme", "usd-2026-10", "sess-a", 33_000, "a2");
    assert_eq!(code, 0, "{at_limit}");
    let (code, over_limit) = sandbox.reserve("acme", "usd-2026-10", "sess-b", 1, "b2");
    assert_eq!(code, 3, "{over_limit}");
}

// ---------------------------------------------------------------------
// 4. No cross-principal contamination.
// ---------------------------------------------------------------------

#[test]
fn cross_principal_pools_and_identity_are_isolated() {
    let sandbox = Sandbox::new();
    sandbox.establish_session("sess-a");
    let acme_feed = sandbox.provision(
        "acme",
        "usd-2026-10",
        libra_governor_domain::QuotaUnit::UsdCents,
        50_000,
    );
    acme_feed.publish_used(0, Some(50_000), OffsetDateTime::now_utc());

    let (code, refused) = sandbox.status("globex", "usd-2026-10");
    assert_eq!(code, 3, "{refused}");
    assert_eq!(refused["reason"]["reason"], "no_pool");

    let globex_feed = sandbox.provision(
        "globex",
        "usd-2026-10",
        libra_governor_domain::QuotaUnit::UsdCents,
        10_000,
    );
    globex_feed.publish_used(0, Some(10_000), OffsetDateTime::now_utc());

    let (code, a) = sandbox.reserve("acme", "usd-2026-10", "sess-a", 20_000, "a1");
    assert_eq!(code, 0, "{a}");
    let a_id = reservation_id(&a);

    let (_, acme_status) = sandbox.status("acme", "usd-2026-10");
    assert_eq!(
        acme_status["active"], 20_000,
        "globex's pool must not affect acme's"
    );

    let (code, wrong_owner) = sandbox.settle(&a_id, "globex", "sess-a", None);
    assert_eq!(code, 3, "{wrong_owner}");
    assert_eq!(wrong_owner["reason"]["reason"], "not_holder");

    let (_, acme_status_after) = sandbox.status("acme", "usd-2026-10");
    assert_eq!(
        acme_status_after["active"], 20_000,
        "a refused settle must never mutate the hold"
    );
}

// ---------------------------------------------------------------------
// 6. No phantom free budget after restart.
// ---------------------------------------------------------------------

#[test]
fn restart_leaves_no_phantom_free_capacity() {
    let sandbox = Sandbox::new();
    sandbox.establish_session("sess-a");
    sandbox.establish_session("sess-b");
    let feed = sandbox.provision(
        "acme",
        "usd-2026-10",
        libra_governor_domain::QuotaUnit::UsdCents,
        50_000,
    );
    feed.publish_used(0, Some(50_000), OffsetDateTime::now_utc());

    let (code, a) = sandbox.reserve("acme", "usd-2026-10", "sess-a", 30_000, "a1");
    assert_eq!(code, 0, "{a}");

    sandbox.state.stop();
    std::thread::sleep(Duration::from_millis(200));

    let (code, b) = sandbox.reserve("acme", "usd-2026-10", "sess-b", 25_000, "b1");
    assert_eq!(
        code, 3,
        "a freshly auto-spawned daemon must not forget acme's 30_000 hold: {b}"
    );
    assert_eq!(b["reason"]["reason"], "insufficient");

    let (_, status) = sandbox.status("acme", "usd-2026-10");
    assert_eq!(status["active"], 30_000);
}

// ---------------------------------------------------------------------
// 7. Stale / unknown snapshot fails closed.
// ---------------------------------------------------------------------

#[test]
fn missing_snapshot_fails_closed() {
    let sandbox = Sandbox::new();
    sandbox.establish_session("sess-a");
    sandbox.provision(
        "acme",
        "usd-2026-10",
        libra_governor_domain::QuotaUnit::UsdCents,
        50_000,
    );
    let (code, refused) = sandbox.reserve("acme", "usd-2026-10", "sess-a", 100, "a1");
    assert_eq!(code, 3, "{refused}");
    assert_eq!(refused["reason"]["reason"], "snapshot");
    assert_eq!(refused["reason"]["state"], "missing");

    let (_, status) = sandbox.status("acme", "usd-2026-10");
    assert_eq!(status["remaining"], serde_json::Value::Null);
}

#[test]
fn undisclosed_snapshot_fails_closed() {
    let sandbox = Sandbox::new();
    sandbox.establish_session("sess-a");
    let feed = sandbox.provision(
        "acme",
        "usd-2026-10",
        libra_governor_domain::QuotaUnit::UsdCents,
        50_000,
    );
    feed.publish_undisclosed(OffsetDateTime::now_utc());
    let (code, refused) = sandbox.reserve("acme", "usd-2026-10", "sess-a", 100, "a1");
    assert_eq!(code, 3, "{refused}");
    assert_eq!(refused["reason"]["state"], "undisclosed");
}

#[test]
fn stale_snapshot_fails_closed() {
    let sandbox = Sandbox::new();
    sandbox.establish_session("sess-a");
    let feed = sandbox.provision(
        "acme",
        "usd-2026-10",
        libra_governor_domain::QuotaUnit::UsdCents,
        50_000,
    );
    feed.publish_stale(OffsetDateTime::now_utc());
    let (code, refused) = sandbox.reserve("acme", "usd-2026-10", "sess-a", 100, "a1");
    assert_eq!(code, 3, "{refused}");
    assert_eq!(refused["reason"]["state"], "stale");
}

#[test]
fn a_declared_limit_below_capacity_lowers_effective_capacity() {
    let sandbox = Sandbox::new();
    sandbox.establish_session("sess-a");
    let feed = sandbox.provision(
        "acme",
        "usd-2026-10",
        libra_governor_domain::QuotaUnit::UsdCents,
        50_000,
    );
    feed.publish_used(0, Some(30_000), OffsetDateTime::now_utc());

    let (code, over) = sandbox.reserve("acme", "usd-2026-10", "sess-a", 30_001, "a1");
    assert_eq!(code, 3, "{over}");
    let (code, ok) = sandbox.reserve("acme", "usd-2026-10", "sess-a", 30_000, "a2");
    assert_eq!(code, 0, "{ok}");

    let (_, status) = sandbox.status("acme", "usd-2026-10");
    assert_eq!(status["effective_capacity"], 30_000);
}

// ---------------------------------------------------------------------
// 8. No false attribution.
// ---------------------------------------------------------------------

#[test]
fn reserve_refuses_an_unestablished_session() {
    let sandbox = Sandbox::new();
    let feed = sandbox.provision(
        "acme",
        "usd-2026-10",
        libra_governor_domain::QuotaUnit::UsdCents,
        50_000,
    );
    feed.publish_used(0, Some(50_000), OffsetDateTime::now_utc());
    let (code, refused) = sandbox.reserve("acme", "usd-2026-10", "never-established", 100, "a1");
    assert_eq!(code, 3, "{refused}");
    assert_eq!(refused["reason"]["reason"], "unknown_session");
}

#[test]
fn two_sessions_reusing_the_same_literal_key_get_distinct_holds() {
    let sandbox = Sandbox::new();
    sandbox.establish_session("sess-a");
    sandbox.establish_session("sess-b");
    let feed = sandbox.provision(
        "acme",
        "usd-2026-10",
        libra_governor_domain::QuotaUnit::UsdCents,
        50_000,
    );
    feed.publish_used(0, Some(50_000), OffsetDateTime::now_utc());

    let (_, a) = sandbox.reserve("acme", "usd-2026-10", "sess-a", 1_000, "shared-key");
    let (_, b) = sandbox.reserve("acme", "usd-2026-10", "sess-b", 1_000, "shared-key");
    assert_ne!(reservation_id(&a), reservation_id(&b));
}

// ---------------------------------------------------------------------
// 9. Unit / owner mismatch fails closed.
// ---------------------------------------------------------------------

#[test]
fn unit_mismatch_against_an_existing_pool_is_refused() {
    let sandbox = Sandbox::new();
    sandbox.establish_session("sess-a");
    let feed = sandbox.provision(
        "acme",
        "usd-2026-10",
        libra_governor_domain::QuotaUnit::UsdCents,
        50_000,
    );
    feed.publish_used(0, Some(50_000), OffsetDateTime::now_utc());

    let (code, refused) = sandbox.metered_pool(&[
        "reserve",
        "--owner",
        "acme",
        "--pool",
        "usd-2026-10",
        "--unit",
        "tokens",
        "--amount",
        "100",
        "--session",
        "sess-a",
        "--key",
        "a1",
    ]);
    assert_eq!(code, 3, "{refused}");
    assert_eq!(refused["reason"]["reason"], "unit_mismatch");
}

#[test]
fn launch_principal_owner_is_refused_as_invalid_pool_key() {
    let sandbox = Sandbox::new();
    sandbox.establish_session("sess-a");
    let (code, refused) = sandbox.reserve("libra-launcher", "usd-2026-10", "sess-a", 100, "a1");
    assert_eq!(code, 3, "{refused}");
    assert_eq!(refused["reason"]["reason"], "invalid_pool_key");
}

// ---------------------------------------------------------------------
// 10. Session cap.
// ---------------------------------------------------------------------

#[test]
fn session_cap_is_a_cooperative_constraint_independent_of_other_sessions() {
    let sandbox = Sandbox::new();
    sandbox.establish_session("sess-a");
    sandbox.establish_session("sess-b");
    let feed = sandbox.provision(
        "acme",
        "usd-2026-10",
        libra_governor_domain::QuotaUnit::UsdCents,
        50_000,
    );
    feed.publish_used(0, Some(50_000), OffsetDateTime::now_utc());

    let (code, a1) = sandbox.reserve_capped("acme", "usd-2026-10", "sess-a", 20_000, "a1", 25_000);
    assert_eq!(code, 0, "{a1}");
    let (code, a2) = sandbox.reserve_capped("acme", "usd-2026-10", "sess-a", 6_000, "a2", 25_000);
    assert_eq!(code, 3, "{a2}");
    assert_eq!(a2["reason"]["reason"], "session_cap_exceeded");
    assert_eq!(a2["reason"]["committed"], 20_000);

    let (code, b1) = sandbox.reserve_capped("acme", "usd-2026-10", "sess-b", 6_000, "b1", 25_000);
    assert_eq!(
        code, 0,
        "sess-b's own cap must be unaffected by sess-a's: {b1}"
    );

    let a1_id = reservation_id(&a1);
    let (code, settled) = sandbox.settle(&a1_id, "acme", "sess-a", Some(12_000));
    assert_eq!(code, 0, "{settled}");
    let (code, a3) = sandbox.reserve_capped("acme", "usd-2026-10", "sess-a", 13_000, "a3", 25_000);
    assert_eq!(
        code, 0,
        "12_000 settled + 13_000 requested == cap 25_000: {a3}"
    );
}

// ---------------------------------------------------------------------
// 5. No oversubscription under concurrency.
// ---------------------------------------------------------------------

#[test]
fn concurrent_reserves_never_collectively_oversubscribe_the_pool() {
    let sandbox = std::sync::Arc::new(Sandbox::new());
    for i in 0..12 {
        sandbox.establish_session(&format!("sess-{i}"));
    }
    let feed = sandbox.provision(
        "acme",
        "usd-2026-10",
        libra_governor_domain::QuotaUnit::UsdCents,
        50_000,
    );
    feed.publish_used(0, Some(50_000), OffsetDateTime::now_utc());

    let handles: Vec<_> = (0..12)
        .map(|i| {
            let sandbox = std::sync::Arc::clone(&sandbox);
            std::thread::spawn(move || {
                let (code, outcome) = sandbox.reserve(
                    "acme",
                    "usd-2026-10",
                    &format!("sess-{i}"),
                    5_000,
                    &format!("k{i}"),
                );
                (code == 0, outcome)
            })
        })
        .collect();

    let mut granted = 0;
    for h in handles {
        let (ok, _outcome) = h.join().unwrap();
        if ok {
            granted += 1;
        }
    }
    assert_eq!(
        granted, 10,
        "exactly 10 of 12 five-thousand-cent requests against a 50_000 pool must be granted"
    );

    let (_, status) = sandbox.status("acme", "usd-2026-10");
    assert_eq!(status["active"], 50_000);
    assert_eq!(status["remaining"], 0);
}
