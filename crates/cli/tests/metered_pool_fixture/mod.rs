//! Test-owned synthetic metered-pool feed (HORO-1915).
//!
//! **SYNTHETIC ONLY.** This fixture opens the daemon's own on-disk SQLite
//! ledger directly (the same multi-process-safe path
//! `hook_cli_integration.rs`'s restart test already uses to read the
//! ledger back) and calls `ensure_pool`/`ingest_pool_provider_snapshot`
//! itself. It is never routed through `libra-governor-quota-observation`'s
//! real registration/consent/trust pipeline, and it is not a production
//! code path — nothing in `libra-governor-daemon`/`libra-governor-cli`
//! ever calls either of those two functions for a metered pool (see
//! `crates/daemon/tests/metered_pool_no_production_feed.rs`'s static
//! guard). This fixture exists solely so the golden-journey test in
//! `metered_pool_golden_journey.rs` can prove the real reserve/settle/
//! release/status CLI path end to end against a known capacity/usage
//! feed — it is not, and must never be mistaken for, a real provider
//! integration.

use time::OffsetDateTime;

use libra_governor_domain::{Confidence, GaugeReading, PoolId, QuotaAmount, QuotaUnit};
use libra_governor_ledger::LedgerStore;

pub struct SyntheticMeteredFeed {
    ledger_path: std::path::PathBuf,
    pool_id: PoolId,
    unit: QuotaUnit,
}

impl SyntheticMeteredFeed {
    /// Opens the daemon's own ledger file under `state_dir` and
    /// provisions a fresh `metered:<owner>/<alias>` pool at `capacity`,
    /// via `ensure_pool` — exactly the same call the (never-production)
    /// static guard in `metered_pool_no_production_feed.rs` confirms has
    /// no caller under `crates/daemon/src` or `crates/cli/src`.
    pub fn provision(
        state_dir: &std::path::Path,
        owner: &str,
        alias: &str,
        unit: QuotaUnit,
        capacity: u64,
    ) -> Self {
        let ledger_path = state_dir.join("ledger.sqlite3");
        let pool_id = PoolId(format!("metered:{owner}/{alias}"));
        let mut store =
            LedgerStore::open(&ledger_path).expect("open the daemon's real ledger file");
        store
            .ensure_pool(&pool_id, &unit, capacity, OffsetDateTime::now_utc())
            .expect("provision synthetic metered pool");
        Self {
            ledger_path,
            pool_id,
            unit,
        }
    }

    fn store(&self) -> LedgerStore {
        LedgerStore::open(&self.ledger_path).expect("reopen the daemon's real ledger file")
    }

    /// Publishes a disclosed `used`/`limit` gauge reading, valid for one
    /// hour from `observed_at`.
    pub fn publish_used(&self, used: u64, limit: Option<u64>, observed_at: OffsetDateTime) {
        self.store()
            .ingest_pool_provider_snapshot(
                &self.pool_id,
                observed_at,
                Some(observed_at + time::Duration::hours(1)),
                &GaugeReading::Used {
                    used: QuotaAmount::new(self.unit.clone(), used),
                    limit,
                },
                Confidence::Medium,
                observed_at,
            )
            .expect("ingest synthetic snapshot");
    }

    /// Publishes a stale (already-expired) snapshot.
    pub fn publish_stale(&self, observed_at: OffsetDateTime) {
        self.store()
            .ingest_pool_provider_snapshot(
                &self.pool_id,
                observed_at,
                Some(observed_at - time::Duration::seconds(1)),
                &GaugeReading::Used {
                    used: QuotaAmount::new(self.unit.clone(), 0),
                    limit: Some(1),
                },
                Confidence::Medium,
                observed_at,
            )
            .expect("ingest synthetic stale snapshot");
    }

    /// Publishes an undisclosed reading.
    pub fn publish_undisclosed(&self, observed_at: OffsetDateTime) {
        self.store()
            .ingest_pool_provider_snapshot(
                &self.pool_id,
                observed_at,
                Some(observed_at + time::Duration::hours(1)),
                &GaugeReading::Undisclosed,
                Confidence::Medium,
                observed_at,
            )
            .expect("ingest synthetic undisclosed snapshot");
    }
}
