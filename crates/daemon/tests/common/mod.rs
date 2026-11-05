//! Helpers shared by integration tests that need a raw view of the ledger file.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use libra_governor_domain::{
    AutonomyBoundary, CompletionContract, CompletionCriterion, Confidence, ConstraintMode, Policy,
    ResourceAmount, ResourceBound, TimeBound,
};

/// Every table's rows as text, read through a separate read-only connection.
pub fn ledger_fingerprint(ledger: &Path) -> String {
    let conn =
        rusqlite::Connection::open_with_flags(ledger, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(Result::unwrap).collect()
    };
    let mut out = String::new();
    for table in tables {
        let mut stmt = conn.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
        let columns = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        out.push_str(&format!("[{table}]\n"));
        while let Some(row) = rows.next().unwrap() {
            for i in 0..columns {
                out.push_str(&format!("{:?}|", row.get_ref(i).unwrap()));
            }
            out.push('\n');
        }
    }
    out
}

pub fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_repo")
}

pub fn generous_policy() -> Policy {
    Policy::validated(
        "generous-test",
        ResourceBound {
            mode: ConstraintMode::Elastic,
            target: ResourceAmount::Tokens(100_000),
            elastic_ceiling: Some(ResourceAmount::Tokens(125_000)),
            hard_ceiling: ResourceAmount::Tokens(150_000),
        },
        TimeBound {
            mode: ConstraintMode::Elastic,
            target_secs: 3600,
            elastic_ceiling_secs: Some(4500),
            hard_ceiling_secs: Some(5400),
            deadline: None,
        },
        CompletionContract::first(vec![CompletionCriterion::required(
            "required verification (tests/build/lint) passes",
        )]),
        Confidence::Low,
        AutonomyBoundary::AskOnApproval,
    )
    .unwrap()
}

/// A `DaemonConfig` over `dir` with no gateway, the given policy and the
/// given shared-budget runtime: what a test needs to drive `handle_connection`.
pub fn daemon_config(
    dir: &Path,
    policy: Policy,
    shared_budget: Arc<libra_governor_daemon::shared_budget::SharedBudgetRuntime>,
) -> libra_governor_daemon::DaemonConfig {
    libra_governor_daemon::DaemonConfig {
        socket_path: dir.join("d.sock"),
        ledger_path: dir.join("ledger.sqlite3"),
        log_path: dir.join("daemon.log"),
        recon_budget: libra_governor_daemon::recon::ReconBudget::default(),
        replan_hysteresis: libra_governor_domain::ReplanHysteresisConfig::default(),
        policy,
        lifetime_scope: libra_governor_domain::LifetimeScope::LifetimeBounded,
        reservation_ttl_secs: 900,
        gateway: None,
        gateway_stats: Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
        extensions: None,
        extension_runtime: std::sync::OnceLock::new(),
        outcome_authority: None,
        progressive_interval_secs: libra_governor_daemon::DEFAULT_PROGRESSIVE_INTERVAL_SECS,
        launch_gate: None,
        shared_budget,
    }
}
