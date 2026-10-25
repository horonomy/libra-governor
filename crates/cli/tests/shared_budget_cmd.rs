//! End-to-end tests of `libra-governor shared-budget prune|export`
//! (HORO-2219, ADR-0026 "Retention and export"). Spawns the real binary
//! against a temp state dir whose ledger the test seeds directly; asserts
//! on real stdout, exit codes and files.
//!
//! Months are derived from the real clock (the binary reads it too), always
//! in UTC, so the tests are stable on any day.

use std::collections::BTreeSet;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use libra_governor_domain::{
    calendar_month_bounds, monthly_pool_alias, ChargeSource, ExternalRef, IanaTimeZone, PoolId,
    PrincipalId, QuotaUnit, TaskIdentity,
};
use libra_governor_ledger::{
    CapacitySource, ExecutorAttribution, LedgerStore, MeteredReserveOutcome, MeteredReserveRequest,
    SharedPoolChargeRequest, SharedPoolReserveRequest,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

fn utc() -> IanaTimeZone {
    IanaTimeZone::new("UTC").unwrap()
}

/// `monthly-YYYY-MM` for the UTC month `months_back` before this one.
fn alias_back(months_back: u32) -> String {
    let (start, _) = calendar_month_bounds(OffsetDateTime::now_utc(), &utc());
    let current = monthly_pool_alias(start, &utc());
    let year: i64 = current[8..12].parse().unwrap();
    let month: i64 = current[13..15].parse().unwrap();
    let index = year * 12 + (month - 1) - i64::from(months_back);
    format!(
        "monthly-{:04}-{:02}",
        index.div_euclid(12),
        index.rem_euclid(12) + 1
    )
}

fn period_of(alias: &str) -> &str {
    &alias["monthly-".len()..]
}

struct Sandbox {
    _parent: tempfile::TempDir,
    state_dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let parent = tempfile::tempdir().unwrap();
        let state_dir = parent.path().join("state");
        Sandbox {
            _parent: parent,
            state_dir,
        }
    }

    fn ledger_path(&self) -> PathBuf {
        self.state_dir.join("ledger.sqlite3")
    }

    fn store(&self) -> LedgerStore {
        std::fs::create_dir_all(&self.state_dir).unwrap();
        std::fs::set_permissions(&self.state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        LedgerStore::open(self.ledger_path()).unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .args(args)
            .env("LIBRA_GOVERNOR_STATE_DIR", &self.state_dir)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

fn pool(alias: &str) -> PoolId {
    PoolId(format!("metered:local-budget/{alias}"))
}

fn owner() -> PrincipalId {
    PrincipalId("local-budget".to_string())
}

fn seed_pool(store: &mut LedgerStore, alias: &str, session: &str) {
    let id = pool(alias);
    let now = OffsetDateTime::now_utc();
    store
        .ensure_pool(&id, &QuotaUnit::UsdCents, 100_000, now)
        .unwrap();
    let cap = CapacitySource::LocalConfigured { cap: 100_000 };
    let attributions = [
        (
            "main",
            ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
            100,
        ),
        (
            "agent",
            ExecutorAttribution::agent("agent-secret-name", Some(ChargeSource::SubagentTranscript)),
            40,
        ),
        (
            "unattr",
            ExecutorAttribution::unattributed(Some(ChargeSource::Gateway)),
            7,
        ),
    ];
    for (key, attribution, amount) in attributions {
        store
            .charge_shared(SharedPoolChargeRequest {
                pool_id: &id,
                principal_id: &owner(),
                session_id: session,
                amount,
                unit: &QuotaUnit::UsdCents,
                idempotency_key: key,
                now,
                attribution,
                capacity_source: cap,
                usage_known: true,
            })
            .unwrap();
    }
}

/// An active hold, so a pool cannot be pruned.
fn seed_active_hold(store: &mut LedgerStore, alias: &str) {
    let now = OffsetDateTime::now_utc();
    let outcome = store
        .reserve_shared_metered(MeteredReserveRequest {
            base: SharedPoolReserveRequest {
                pool_id: &pool(alias),
                principal_id: &owner(),
                session_id: "holder",
                amount: 5,
                unit: &QuotaUnit::UsdCents,
                idempotency_key: "hold",
                now,
                ttl_secs: 3600,
                attribution: ExecutorAttribution::default(),
            },
            session_cap: None,
            capacity_source: CapacitySource::LocalConfigured { cap: 100_000 },
        })
        .unwrap();
    assert!(matches!(outcome, MeteredReserveOutcome::Granted(_)));
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

// ---------------------------------------------------------------------
// prune
// ---------------------------------------------------------------------

#[test]
fn prune_is_a_dry_run_unless_apply_is_given() {
    let sandbox = Sandbox::new();
    let mut store = sandbox.store();
    let old = alias_back(5);
    let recent = alias_back(1);
    let current = alias_back(0);
    for alias in [&old, &recent, &current] {
        seed_pool(&mut store, alias, "s");
    }
    drop(store);

    let dry = sandbox.run(&[
        "shared-budget",
        "prune",
        "--keep-months",
        "2",
        "--time-zone",
        "UTC",
    ]);
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let report = &stdout_json(&dry)["report"];
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["pools_considered"], 3);
    assert_eq!(report["pools_selected"], 1);
    assert_eq!(report["pools_pruned"], 0);
    assert_eq!(report["rows_selected"], 3);
    assert_eq!(report["rows_deleted"], 0);
    assert_eq!(report["rows_kept"], 9);
    assert!(sandbox.store().quota_pool(&pool(&old)).unwrap().is_some());

    let applied = sandbox.run(&[
        "shared-budget",
        "prune",
        "--keep-months",
        "2",
        "--time-zone",
        "UTC",
        "--apply",
    ]);
    assert!(applied.status.success());
    let body = stdout_json(&applied);
    assert_eq!(body["report"]["dry_run"], false);
    assert_eq!(body["report"]["pools_pruned"], 1);
    assert_eq!(body["report"]["rows_deleted"], 3);
    assert_eq!(body["report"]["rows_kept"], 6);
    assert!(body["note"]
        .as_str()
        .unwrap()
        .contains("can no longer be exported"));
    let store = sandbox.store();
    assert!(store.quota_pool(&pool(&old)).unwrap().is_none());
    assert!(store.quota_pool(&pool(&recent)).unwrap().is_some());
    assert!(store.quota_pool(&pool(&current)).unwrap().is_some());

    let again = sandbox.run(&[
        "shared-budget",
        "prune",
        "--keep-months",
        "2",
        "--time-zone",
        "UTC",
        "--apply",
    ]);
    assert_eq!(stdout_json(&again)["report"]["rows_deleted"], 0);
}

#[test]
fn prune_never_removes_a_pool_with_an_active_hold() {
    let sandbox = Sandbox::new();
    let mut store = sandbox.store();
    let old = alias_back(6);
    seed_pool(&mut store, &old, "s");
    seed_active_hold(&mut store, &old);
    drop(store);
    let out = sandbox.run(&[
        "shared-budget",
        "prune",
        "--keep-months",
        "1",
        "--time-zone",
        "UTC",
        "--apply",
    ]);
    assert!(out.status.success());
    let report = &stdout_json(&out)["report"];
    assert_eq!(report["pools_pruned"], 0);
    assert_eq!(report["rows_deleted"], 0);
    assert_eq!(report["pools"][0]["disposition"], "skipped_active_hold");
}

#[test]
fn prune_refuses_bad_arguments_with_exit_2() {
    let sandbox = Sandbox::new();
    for args in [
        vec![
            "shared-budget",
            "prune",
            "--keep-months",
            "0",
            "--time-zone",
            "UTC",
        ],
        vec![
            "shared-budget",
            "prune",
            "--keep-months",
            "-1",
            "--time-zone",
            "UTC",
        ],
        vec![
            "shared-budget",
            "prune",
            "--keep-months",
            "x",
            "--time-zone",
            "UTC",
        ],
        vec!["shared-budget", "prune", "--time-zone", "UTC"],
        vec!["shared-budget", "prune", "--keep-months", "1"],
        vec![
            "shared-budget",
            "prune",
            "--keep-months",
            "1",
            "--time-zone",
            "Mars/Olympus",
        ],
        vec![
            "shared-budget",
            "prune",
            "--keep-months",
            "1",
            "--time-zone",
            "UTC",
            "--force",
        ],
        vec!["shared-budget", "nope"],
        vec!["shared-budget"],
    ] {
        let out = sandbox.run(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
    assert!(!sandbox.state_dir.exists(), "refusals leave no trace");
}

#[test]
fn prune_without_a_ledger_reports_nothing_and_creates_nothing() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&[
        "shared-budget",
        "prune",
        "--keep-months",
        "1",
        "--time-zone",
        "UTC",
        "--apply",
    ]);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out)["report"]["pools_considered"], 0);
    assert!(!sandbox.ledger_path().exists());
}

// ---------------------------------------------------------------------
// export
// ---------------------------------------------------------------------

const MARKER: &str = "MARKER-HORO2219-prompt-like-nonce";

fn seeded_export_sandbox() -> (Sandbox, String) {
    let sandbox = Sandbox::new();
    let mut store = sandbox.store();
    let alias = alias_back(1);
    seed_pool(&mut store, &alias, &format!("sess-{MARKER}"));
    // Content-bearing data elsewhere in the same ledger must never surface.
    store
        .insert_task(
            &TaskIdentity::new(Some(ExternalRef::Other(format!("fix login {MARKER}")))),
            OffsetDateTime::now_utc(),
        )
        .unwrap();
    (sandbox, alias)
}

#[test]
fn export_writes_the_documented_schema_with_a_verifiable_hash_and_private_perms() {
    let (sandbox, alias) = seeded_export_sandbox();
    let period = period_of(&alias);
    let out = sandbox.run(&["shared-budget", "export", "--period", period, "--raw-ids"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let summary = stdout_json(&out);
    let path = PathBuf::from(summary["written"].as_str().unwrap());
    assert!(path.starts_with(sandbox.state_dir.join("exports")));

    let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    let dir_mode = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(file_mode, 0o600);
    assert_eq!(dir_mode, 0o700);

    let doc = read_json(&path);
    assert_eq!(
        keys(&doc),
        set(&["body", "body_sha256", "canonicalization", "generated_at"])
    );
    let body = &doc["body"];
    assert_eq!(
        keys(body),
        set(&[
            "basis",
            "executors",
            "guarantee",
            "id_mode",
            "period",
            "pool",
            "schema",
            "totals",
            "transport_policy",
        ])
    );
    assert_eq!(body["schema"], "libra.shared-budget-export.v1");
    assert_eq!(body["basis"], "libra-accounted-usd-list-price-equivalent");
    assert_eq!(body["transport_policy"], "local_only");
    assert_eq!(body["id_mode"], "raw");
    assert_eq!(
        keys(&body["period"]),
        set(&[
            "end_local",
            "end_utc",
            "label",
            "start_local",
            "start_utc",
            "time_zone"
        ])
    );
    assert_eq!(body["period"]["label"], period);
    assert_eq!(keys(&body["pool"]), set(&["capacity", "pool_id", "unit"]));
    assert_eq!(body["pool"]["capacity"], 100_000);
    assert_eq!(body["totals"]["used"], 147);
    assert_eq!(body["totals"]["reserved"], 0);
    assert_eq!(body["totals"]["executors"], 3);

    let executors = body["executors"].as_array().unwrap();
    assert_eq!(executors.len(), 3);
    for e in executors {
        assert_eq!(
            keys(e),
            set(&[
                "agent_id",
                "by_charge_source",
                "executor_kind",
                "reserved",
                "rows",
                "session_id",
                "used",
            ])
        );
        assert_eq!(
            keys(&e["rows"]),
            set(&["active", "expired", "released", "settled"])
        );
    }
    let sum: u64 = executors.iter().map(|e| e["used"].as_u64().unwrap()).sum();
    assert_eq!(sum, body["totals"]["used"].as_u64().unwrap());
    let kinds: BTreeSet<_> = executors
        .iter()
        .map(|e| e["executor_kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, set(&["agent", "agent_absent", "unattributed"]));
    let agent = executors
        .iter()
        .find(|e| e["executor_kind"] == "agent")
        .unwrap();
    assert_eq!(agent["agent_id"], "agent-secret-name");
    let absent = executors
        .iter()
        .find(|e| e["executor_kind"] == "agent_absent")
        .unwrap();
    assert!(absent["agent_id"].is_null());

    // The digest is the SHA-256 of the compact, key-sorted body.
    let canonical = serde_json::to_vec(body).unwrap();
    let expected: String = Sha256::digest(&canonical)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(doc["body_sha256"], expected.as_str());
    assert_eq!(summary["body_sha256"], doc["body_sha256"]);
}

#[test]
fn export_hash_is_deterministic_and_excludes_generation_time() {
    let (sandbox, alias) = seeded_export_sandbox();
    let period = period_of(&alias);
    let first = sandbox.run(&["shared-budget", "export", "--period", period]);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let second = sandbox.run(&["shared-budget", "export", "--period", period, "--overwrite"]);
    assert!(first.status.success() && second.status.success());
    let a = stdout_json(&first);
    let b = stdout_json(&second);
    assert_eq!(a["body_sha256"], b["body_sha256"]);
    assert_eq!(a["written"], b["written"]);
}

#[test]
fn export_hash_changes_when_the_ledger_changes() {
    let (sandbox, alias) = seeded_export_sandbox();
    let period = period_of(&alias);
    let before = stdout_json(&sandbox.run(&["shared-budget", "export", "--period", period]));
    let mut store = sandbox.store();
    store
        .charge_shared(SharedPoolChargeRequest {
            pool_id: &pool(&alias),
            principal_id: &owner(),
            session_id: "later",
            amount: 1,
            unit: &QuotaUnit::UsdCents,
            idempotency_key: "later",
            now: OffsetDateTime::now_utc(),
            attribution: ExecutorAttribution::default(),
            capacity_source: CapacitySource::LocalConfigured { cap: 100_000 },
            usage_known: true,
        })
        .unwrap();
    drop(store);
    let after =
        stdout_json(&sandbox.run(&["shared-budget", "export", "--period", period, "--overwrite"]));
    assert_ne!(before["body_sha256"], after["body_sha256"]);
}

#[test]
fn export_redacts_ids_by_default_and_never_leaks_content() {
    let (sandbox, alias) = seeded_export_sandbox();
    let period = period_of(&alias);
    let out = sandbox.run(&["shared-budget", "export", "--period", period]);
    assert!(out.status.success());
    let summary = stdout_json(&out);
    let path = PathBuf::from(summary["written"].as_str().unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    let doc: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(doc["body"]["id_mode"], "redacted");

    for needle in [MARKER, "agent-secret-name", "sess-"] {
        assert!(!text.contains(needle), "export leaks {needle:?}");
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains(needle),
            "stdout leaks {needle:?}"
        );
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains(needle),
            "stderr leaks {needle:?}"
        );
    }
    for e in doc["body"]["executors"].as_array().unwrap() {
        assert!(e["session_id"].as_str().unwrap().starts_with("session_id:"));
        if e["executor_kind"] == "agent" {
            assert!(e["agent_id"].as_str().unwrap().starts_with("agent_id:"));
        }
    }

    // The raw-id opt-out is the only thing that surfaces the session id,
    // and even then the unrelated task content never appears.
    let raw = sandbox.run(&[
        "shared-budget",
        "export",
        "--period",
        period,
        "--raw-ids",
        "--out",
        sandbox.state_dir.join("raw.json").to_str().unwrap(),
    ]);
    assert!(raw.status.success());
    let raw_text = std::fs::read_to_string(sandbox.state_dir.join("raw.json")).unwrap();
    assert!(raw_text.contains(&format!("sess-{MARKER}")));
    assert!(!raw_text.contains("fix login"));
    for forbidden in [
        "prompt",
        "transcript",
        "tool_input",
        "tool_output",
        "source_code",
        "cwd",
        "path",
    ] {
        assert!(!text.contains(&format!("\"{forbidden}")), "key {forbidden}");
    }
}

#[test]
fn export_refuses_to_clobber_and_replaces_only_with_overwrite() {
    let (sandbox, alias) = seeded_export_sandbox();
    let period = period_of(&alias);
    let target = sandbox.state_dir.join("mine.json");
    std::fs::write(&target, b"precious").unwrap();
    let args = [
        "shared-budget",
        "export",
        "--period",
        period,
        "--out",
        target.to_str().unwrap(),
    ];
    let refused = sandbox.run(&args);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(std::fs::read(&target).unwrap(), b"precious");

    let mut with_overwrite = args.to_vec();
    with_overwrite.push("--overwrite");
    let ok = sandbox.run(&with_overwrite);
    assert!(ok.status.success());
    assert_ne!(std::fs::read(&target).unwrap(), b"precious");
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn export_refuses_non_local_or_non_regular_targets() {
    let (sandbox, alias) = seeded_export_sandbox();
    let period = period_of(&alias);
    for url in ["s3://bucket/key.json", "file:///tmp/x.json", "-"] {
        let out = sandbox.run(&["shared-budget", "export", "--period", period, "--out", url]);
        assert_eq!(out.status.code(), Some(2), "{url}");
    }
    let dir = sandbox.state_dir.join("a-directory");
    std::fs::create_dir(&dir).unwrap();
    let elsewhere = sandbox.state_dir.join("elsewhere.json");
    std::fs::write(&elsewhere, b"keep").unwrap();
    let link = sandbox.state_dir.join("link.json");
    symlink(&elsewhere, &link).unwrap();
    for target in [&dir, &link] {
        for overwrite in [false, true] {
            let mut args = vec![
                "shared-budget",
                "export",
                "--period",
                period,
                "--out",
                target.to_str().unwrap(),
            ];
            if overwrite {
                args.push("--overwrite");
            }
            let out = sandbox.run(&args);
            assert_eq!(
                out.status.code(),
                Some(1),
                "{target:?} overwrite={overwrite}"
            );
        }
    }
    assert_eq!(std::fs::read(&elsewhere).unwrap(), b"keep");
}

#[test]
fn export_of_a_missing_period_or_ledger_fails_clearly_and_writes_nothing() {
    let (sandbox, alias) = seeded_export_sandbox();
    let missing = period_of(&alias_back(9)).to_string();
    let out = sandbox.run(&["shared-budget", "export", "--period", &missing]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no shared-budget pool exists"), "{stderr}");
    assert!(!sandbox.state_dir.join("exports").exists());
    let _ = alias;

    let empty = Sandbox::new();
    let out = empty.run(&["shared-budget", "export", "--period", "2026-01"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no ledger"));
    assert!(!empty.state_dir.exists());

    for bad in ["2026-1", "2026-13", "garbage"] {
        let out = sandbox.run(&["shared-budget", "export", "--period", bad]);
        assert_eq!(out.status.code(), Some(2), "{bad}");
    }
}

// ---------------------------------------------------------------------
// time zone follows shared-budget.json; read-only dry run; wording
// ---------------------------------------------------------------------

const GOOD_CONFIG: &str =
    r#"{"schema_version":1,"monthly_cap_usd_cents":50000,"time_zone":"America/New_York"}"#;

fn write_config(sandbox: &Sandbox, body: &str) {
    std::fs::create_dir_all(&sandbox.state_dir).unwrap();
    std::fs::write(sandbox.state_dir.join("shared-budget.json"), body).unwrap();
}

#[test]
fn prune_zone_defaults_to_the_configured_zone_and_refuses_a_mismatch() {
    let sandbox = Sandbox::new();
    write_config(&sandbox, GOOD_CONFIG);

    let defaulted = sandbox.run(&["shared-budget", "prune", "--keep-months", "1"]);
    assert!(
        defaulted.status.success(),
        "{}",
        String::from_utf8_lossy(&defaulted.stderr)
    );
    assert_eq!(stdout_json(&defaulted)["time_zone"], "America/New_York");

    let same = sandbox.run(&[
        "shared-budget",
        "prune",
        "--keep-months",
        "1",
        "--time-zone",
        "America/New_York",
    ]);
    assert!(same.status.success());

    let differs = sandbox.run(&[
        "shared-budget",
        "prune",
        "--keep-months",
        "1",
        "--time-zone",
        "UTC",
        "--apply",
    ]);
    assert_eq!(differs.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&differs.stderr).contains("differs from shared-budget.json"));
}

#[test]
fn prune_zone_is_required_without_a_valid_config() {
    let sandbox = Sandbox::new();
    let none = sandbox.run(&["shared-budget", "prune", "--keep-months", "1"]);
    assert_eq!(none.status.code(), Some(2));

    write_config(&sandbox, "{ not json");
    let invalid = sandbox.run(&["shared-budget", "prune", "--keep-months", "1"]);
    assert_eq!(invalid.status.code(), Some(2));
    let with_flag = sandbox.run(&[
        "shared-budget",
        "prune",
        "--keep-months",
        "1",
        "--time-zone",
        "UTC",
    ]);
    assert!(with_flag.status.success());
}

#[test]
fn prune_note_carries_the_export_first_and_replay_warning() {
    let sandbox = Sandbox::new();
    for apply in [false, true] {
        let mut args = vec![
            "shared-budget",
            "prune",
            "--keep-months",
            "1",
            "--time-zone",
            "UTC",
        ];
        if apply {
            args.push("--apply");
        }
        let note = stdout_json(&sandbox.run(&args))["note"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(note.contains("Export before prune"), "{note}");
        assert!(note.contains("can no longer be exported"), "{note}");
        assert!(
            note.contains("older than the oldest retained period is refused by the daemon"),
            "{note}"
        );
    }
    let help = sandbox.run(&["shared-budget"]);
    let text = String::from_utf8_lossy(&help.stderr).to_string();
    assert!(text.contains("is refused by the daemon"));
    assert!(text.contains("pseudonymised"));
    assert!(text.contains("not safe to publish"));
}

#[test]
fn dry_run_and_export_do_not_migrate_an_older_ledger_but_apply_would() {
    let sandbox = Sandbox::new();
    let mut store = sandbox.store();
    let alias = alias_back(5);
    seed_pool(&mut store, &alias, "s");
    drop(store);
    // Make the schema look one migration behind.
    let conn = rusqlite::Connection::open(sandbox.ledger_path()).unwrap();
    conn.execute(
        "DELETE FROM schema_migrations WHERE version = (SELECT MAX(version) FROM schema_migrations)",
        [],
    )
    .unwrap();
    drop(conn);

    let dry = sandbox.run(&[
        "shared-budget",
        "prune",
        "--keep-months",
        "1",
        "--time-zone",
        "UTC",
    ]);
    assert_eq!(dry.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&dry.stderr).contains("schema version"));
    let export = sandbox.run(&["shared-budget", "export", "--period", period_of(&alias)]);
    assert_eq!(export.status.code(), Some(1));

    let conn = rusqlite::Connection::open(sandbox.ledger_path()).unwrap();
    let max: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        max,
        libra_governor_ledger::latest_known_version() - 1,
        "no migration ran"
    );
}

#[test]
fn export_refuses_a_symlinked_default_exports_directory() {
    let (sandbox, alias) = seeded_export_sandbox();
    let target = sandbox.state_dir.join("elsewhere");
    std::fs::create_dir(&target).unwrap();
    symlink(&target, sandbox.state_dir.join("exports")).unwrap();
    let out = sandbox.run(&["shared-budget", "export", "--period", period_of(&alias)]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
}
