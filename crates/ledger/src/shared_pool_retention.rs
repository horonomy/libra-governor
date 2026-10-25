//! Retention prune and local export read model for the local monthly
//! shared budget (HORO-2219, ADR-0026 "Retention and export").
//!
//! # Prune safety rules
//!
//! Only pools of owner [`LOCAL_BUDGET_OWNER`] are ever considered, and only
//! when the alias is exactly `monthly-YYYY-MM`. A pool is deleted only when
//! all of these hold, re-checked inside the same `BEGIN IMMEDIATE`
//! transaction that deletes it:
//!
//! - its period END is at or before the start of the zone-local month that
//!   is `keep_months` months before the current zone-local month (so with
//!   `keep_months = 1` the previous closed month is kept and everything
//!   older goes; the current and any future-dated period are never touched);
//! - it holds no row in state `active`. This is deliberately stricter than
//!   "unexpired": an `active` row whose expiry has passed but has not yet
//!   been reaped by `expire_stale_shared_pool_reservations` still blocks
//!   pruning, so a pool is never deleted out from under accounting that has
//!   not reached a terminal state.
//!
//! A dry run and an export need no write access: callers can serve them
//! from [`LedgerStore::open_read_only`], which neither migrates nor
//! checkpoints.
//!
//! One transaction per pool keeps every write short, so a prune running
//! next to a daemon contends only for the SQLite write lock (bounded by the
//! connection's `busy_timeout`), never for a long hold. The report carries
//! counts and pool aliases only -- never row content.

use std::collections::BTreeMap;

use libra_governor_domain::{
    calendar_month_bounds, monthly_pool_alias, ExecutorKind, IanaTimeZone, MeteredPoolKey, PoolId,
    QuotaUnit, LOCAL_BUDGET_OWNER, METERED_POOL_PREFIX,
};
use rusqlite::TransactionBehavior;
use serde::Serialize;
use time::OffsetDateTime;

use crate::{store::LedgerStore, transaction::begin, LedgerError};

/// Why a retention request was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum RetentionError {
    /// `keep_months` must be at least 1: zero would make the current month
    /// eligible for deletion.
    #[error("keep_months must be at least 1")]
    KeepMonthsZero,
    #[error(transparent)]
    Ledger(#[from] LedgerError),
}

impl From<rusqlite::Error> for RetentionError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Ledger(e.into())
    }
}

/// Input to [`LedgerStore::prune_closed_periods`].
#[derive(Debug, Clone, Copy)]
pub struct PruneRequest<'a> {
    /// Closed months to keep in addition to the current one. At least 1.
    pub keep_months: u32,
    pub now: OffsetDateTime,
    pub time_zone: &'a IanaTimeZone,
    /// Select and count, but delete and write nothing.
    pub dry_run: bool,
}

/// What happened to (or would happen to) one pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PruneDisposition {
    /// Deleted (apply mode).
    Pruned,
    /// Eligible; nothing deleted (dry-run mode).
    WouldPrune,
    /// The current, a future, or a recent-enough closed period.
    Retained,
    /// Old enough but holds an `active` row; left alone.
    SkippedActiveHold,
    /// A `local-budget` pool whose alias is not `monthly-YYYY-MM`.
    SkippedUnrecognisedAlias,
}

/// Per-pool counts. `pool_id` is a pool alias form, never row content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PoolPruneDetail {
    pub pool_id: String,
    pub disposition: PruneDisposition,
    /// Rows deleted (always 0 in dry-run mode).
    pub rows_deleted: u64,
    /// Rows that remain in the ledger for this pool.
    pub rows_kept: u64,
    /// Rows a prune would delete (dry-run) or did delete (apply).
    pub rows_selected: u64,
}

/// Counts-only result of a prune.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PruneReport {
    pub dry_run: bool,
    pub keep_months: u32,
    /// `local-budget` pools examined.
    pub pools_considered: u64,
    /// Pools eligible for pruning (equals `pools_pruned` when applied).
    pub pools_selected: u64,
    pub pools_pruned: u64,
    pub rows_selected: u64,
    pub rows_deleted: u64,
    pub rows_kept: u64,
    pub pools: Vec<PoolPruneDetail>,
}

/// `YYYY-MM` -> months since year 0, or `None` for anything that is not
/// exactly `monthly-YYYY-MM` with a real month.
fn month_index_of_alias(alias: &str) -> Option<i64> {
    let rest = alias.strip_prefix("monthly-")?;
    let (year, month) = rest.split_once('-')?;
    if year.len() != 4 || month.len() != 2 {
        return None;
    }
    if !year.bytes().all(|b| b.is_ascii_digit()) || !month.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: i64 = year.parse().ok()?;
    let month: i64 = month.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    Some(year * 12 + (month - 1))
}

/// Whether the period at `pool_index` is old enough to prune: its end
/// (start of `pool_index + 1`) is at or before the start of the month
/// `keep_months` before `current_index`.
fn is_prunable_period(pool_index: i64, current_index: i64, keep_months: u32) -> bool {
    // end index (pool_index + 1) <= threshold index, written without the +1.
    pool_index < current_index - i64::from(keep_months)
}

fn pool_counts(
    conn: &rusqlite::Connection,
    pool_id: &str,
) -> Result<(u64, u64, u64), rusqlite::Error> {
    // (total rows, closed rows, active rows)
    conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN state IN ('settled', 'released', 'expired') THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state = 'active' THEN 1 ELSE 0 END), 0)
         FROM shared_pool_reservations WHERE pool_id = ?1",
        [pool_id],
        |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)? as u64,
            ))
        },
    )
}

impl LedgerStore {
    /// Prunes closed `local-budget` monthly pools older than the retention
    /// window. See the module docs for the exact rules. Each pool is
    /// handled in its own `BEGIN IMMEDIATE` transaction; a dry run uses a
    /// read-only snapshot per pool and writes nothing.
    pub fn prune_closed_periods(
        &mut self,
        req: PruneRequest<'_>,
    ) -> Result<PruneReport, RetentionError> {
        if req.keep_months == 0 {
            return Err(RetentionError::KeepMonthsZero);
        }
        let (current_start, _) = calendar_month_bounds(req.now, req.time_zone);
        let current_alias = monthly_pool_alias(current_start, req.time_zone);
        let current_index = month_index_of_alias(&current_alias)
            .ok_or(LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;

        let prefix = format!("{METERED_POOL_PREFIX}{LOCAL_BUDGET_OWNER}/");
        let pool_ids: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT pool_id FROM quota_pools
                 WHERE substr(pool_id, 1, length(?1)) = ?1
                 ORDER BY pool_id",
            )?;
            let rows = stmt.query_map([&prefix], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };

        let mut report = PruneReport {
            dry_run: req.dry_run,
            keep_months: req.keep_months,
            pools_considered: 0,
            pools_selected: 0,
            pools_pruned: 0,
            rows_selected: 0,
            rows_deleted: 0,
            rows_kept: 0,
            pools: Vec::new(),
        };

        for pool_id in pool_ids {
            report.pools_considered += 1;
            let key = MeteredPoolKey::parse(&PoolId(pool_id.clone()));
            let pool_index = key
                .filter(|k| k.owner().0 == LOCAL_BUDGET_OWNER)
                .and_then(|k| month_index_of_alias(k.alias()));
            let detail = match pool_index {
                None => {
                    let (total, _, _) = pool_counts(&self.conn, &pool_id)?;
                    PoolPruneDetail {
                        pool_id,
                        disposition: PruneDisposition::SkippedUnrecognisedAlias,
                        rows_deleted: 0,
                        rows_kept: total,
                        rows_selected: 0,
                    }
                }
                Some(index) if !is_prunable_period(index, current_index, req.keep_months) => {
                    let (total, _, _) = pool_counts(&self.conn, &pool_id)?;
                    PoolPruneDetail {
                        pool_id,
                        disposition: PruneDisposition::Retained,
                        rows_deleted: 0,
                        rows_kept: total,
                        rows_selected: 0,
                    }
                }
                Some(_) => self.prune_one_pool(pool_id, req.dry_run)?,
            };
            if matches!(
                detail.disposition,
                PruneDisposition::Pruned | PruneDisposition::WouldPrune
            ) {
                report.pools_selected += 1;
            }
            if detail.disposition == PruneDisposition::Pruned {
                report.pools_pruned += 1;
            }
            report.rows_selected += detail.rows_selected;
            report.rows_deleted += detail.rows_deleted;
            report.rows_kept += detail.rows_kept;
            report.pools.push(detail);
        }
        Ok(report)
    }

    /// Prunes (or, for a dry run, only evaluates) one already-age-eligible
    /// pool. The active-hold check and the deletes share one transaction.
    fn prune_one_pool(
        &mut self,
        pool_id: String,
        dry_run: bool,
    ) -> Result<PoolPruneDetail, RetentionError> {
        if dry_run {
            let tx = self.conn.unchecked_transaction()?;
            let (total, closed, active) = pool_counts(&tx, &pool_id)?;
            drop(tx);
            return Ok(if active > 0 {
                PoolPruneDetail {
                    pool_id,
                    disposition: PruneDisposition::SkippedActiveHold,
                    rows_deleted: 0,
                    rows_kept: total,
                    rows_selected: 0,
                }
            } else {
                PoolPruneDetail {
                    pool_id,
                    disposition: PruneDisposition::WouldPrune,
                    rows_deleted: 0,
                    rows_kept: total,
                    rows_selected: closed,
                }
            });
        }

        let tx = begin(&mut self.conn, TransactionBehavior::Immediate)?;
        let (total, closed, active) = pool_counts(&tx, &pool_id)?;
        if active > 0 {
            tx.commit()?;
            return Ok(PoolPruneDetail {
                pool_id,
                disposition: PruneDisposition::SkippedActiveHold,
                rows_deleted: 0,
                rows_kept: total,
                rows_selected: 0,
            });
        }
        // Keep each subagent's cumulative charged total (HORO-2250): the
        // idempotency keys of the rows below are its only record, and a
        // replay or a grown transcript must not be booked again. The family
        // prefix is the key up to its last '|' (rtrim strips the trailing
        // non-'|' characters).
        tx.execute(
            "INSERT INTO shared_charge_floor (principal_id, key_prefix, total)
             SELECT principal_id,
                    rtrim(idempotency_key, replace(idempotency_key, '|', '')),
                    SUM(settled_amount)
             FROM shared_pool_reservations
             WHERE pool_id = ?1 AND is_charge = 1 AND state = 'settled'
               AND settled_amount IS NOT NULL
               AND substr(idempotency_key, 1, 20) = 'subagent-transcript:'
             GROUP BY principal_id, rtrim(idempotency_key, replace(idempotency_key, '|', ''))
             ON CONFLICT (principal_id, key_prefix)
             DO UPDATE SET total = total + excluded.total",
            [&pool_id],
        )?;
        let deleted = tx.execute(
            "DELETE FROM shared_pool_reservations
             WHERE pool_id = ?1 AND state IN ('settled', 'released', 'expired')",
            [&pool_id],
        )? as u64;
        // The provider snapshot is a latest-gauge row keyed by pool; it
        // references the pool and carries no spend.
        tx.execute(
            "DELETE FROM shared_pool_provider_snapshots WHERE pool_id = ?1",
            [&pool_id],
        )?;
        // Fails (rolling the whole pool back) if any reservation row is
        // left, which the state CHECK makes impossible after the delete.
        tx.execute("DELETE FROM quota_pools WHERE pool_id = ?1", [&pool_id])?;
        tx.commit()?;
        debug_assert_eq!(deleted, closed);
        Ok(PoolPruneDetail {
            pool_id,
            disposition: PruneDisposition::Pruned,
            rows_deleted: deleted,
            rows_kept: 0,
            rows_selected: deleted,
        })
    }
}

/// Spend and hold totals for one `charge_source` (rows with none recorded
/// are keyed `unrecorded`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ChargeSourceTotals {
    pub used: u64,
    pub reserved: u64,
    pub rows: u64,
}

/// One executor's aggregate in an export. Never carries content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolExportExecutor {
    pub session_id: String,
    pub agent_id: Option<String>,
    pub executor_kind: ExecutorKind,
    /// Settled amount.
    pub used: u64,
    /// Active-hold amount.
    pub reserved: u64,
    pub rows_settled: u64,
    pub rows_active: u64,
    pub rows_released: u64,
    pub rows_expired: u64,
    pub by_charge_source: BTreeMap<String, ChargeSourceTotals>,
}

/// A consistent single-snapshot aggregate of one pool for export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolExport {
    pub pool_id: PoolId,
    pub unit: QuotaUnit,
    pub capacity: u64,
    pub executors: Vec<PoolExportExecutor>,
    pub total_used: u64,
    pub total_reserved: u64,
    pub total_rows: u64,
}

type ExecutorKey = (String, Option<String>, ExecutorKind);

impl LedgerStore {
    /// Aggregates `pool_id` per executor for a local export, in one read
    /// snapshot. `None` if the pool does not exist. Totals are the sums of
    /// the executor rows, so they cannot disagree with them.
    pub fn shared_pool_export(&self, pool_id: &PoolId) -> Result<Option<PoolExport>, LedgerError> {
        let tx = self.conn.unchecked_transaction()?;
        let pool_row: Option<(String, i64)> = {
            use rusqlite::OptionalExtension;
            tx.query_row(
                "SELECT unit_json, capacity FROM quota_pools WHERE pool_id = ?1",
                [&pool_id.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
        };
        let Some((unit_json, capacity)) = pool_row else {
            return Ok(None);
        };
        let unit: QuotaUnit = serde_json::from_str(&unit_json)
            .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;

        let mut by_key: BTreeMap<ExecutorKey, PoolExportExecutor> = BTreeMap::new();
        {
            let mut stmt = tx.prepare(
                "SELECT session_id, agent_id,
                        COALESCE(executor_kind, 'unattributed'),
                        COALESCE(charge_source, 'unrecorded'),
                        state, COUNT(*),
                        COALESCE(SUM(amount), 0),
                        COALESCE(SUM(settled_amount), 0)
                 FROM shared_pool_reservations
                 WHERE pool_id = ?1
                 GROUP BY session_id, agent_id, 3, 4, state",
            )?;
            let rows = stmt.query_map([&pool_id.0], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })?;
            for row in rows {
                let (session, agent, kind, source, state, count, amount, settled) = row?;
                let kind = ExecutorKind::parse(&kind)
                    .ok_or(LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
                let entry = by_key
                    .entry((session.clone(), agent.clone(), kind))
                    .or_insert_with(|| PoolExportExecutor {
                        session_id: session,
                        agent_id: agent,
                        executor_kind: kind,
                        used: 0,
                        reserved: 0,
                        rows_settled: 0,
                        rows_active: 0,
                        rows_released: 0,
                        rows_expired: 0,
                        by_charge_source: BTreeMap::new(),
                    });
                let count = count.max(0) as u64;
                let totals = entry.by_charge_source.entry(source).or_default();
                totals.rows += count;
                match state.as_str() {
                    "settled" => {
                        entry.rows_settled += count;
                        entry.used += settled.max(0) as u64;
                        totals.used += settled.max(0) as u64;
                    }
                    "active" => {
                        entry.rows_active += count;
                        entry.reserved += amount.max(0) as u64;
                        totals.reserved += amount.max(0) as u64;
                    }
                    "released" => entry.rows_released += count,
                    "expired" => entry.rows_expired += count,
                    _ => return Err(LedgerError::Sqlite(rusqlite::Error::InvalidQuery)),
                }
            }
        }
        drop(tx);

        let executors: Vec<PoolExportExecutor> = by_key.into_values().collect();
        let total_used = executors.iter().map(|e| e.used).sum();
        let total_reserved = executors.iter().map(|e| e.reserved).sum();
        let total_rows = executors
            .iter()
            .map(|e| e.rows_settled + e.rows_active + e.rows_released + e.rows_expired)
            .sum();
        Ok(Some(PoolExport {
            pool_id: pool_id.clone(),
            unit,
            capacity: capacity.max(0) as u64,
            executors,
            total_used,
            total_reserved,
            total_rows,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_parser_accepts_only_the_exact_form() {
        assert_eq!(month_index_of_alias("monthly-2026-09"), Some(2026 * 12 + 8));
        for bad in [
            "monthly-2026-9",
            "monthly-26-09",
            "monthly-2026-13",
            "monthly-2026-00",
            "monthly-2026-09-",
            "Monthly-2026-09",
            "monthly-2026-09x",
            "monthly--202-09",
            "monthly-+026-09",
            "weekly-2026-09",
            "",
        ] {
            assert_eq!(month_index_of_alias(bad), None, "{bad}");
        }
    }

    #[test]
    fn prunable_boundary_is_exactly_n_closed_months_kept() {
        let cur = 2026 * 12 + 9; // 2026-10
                                 // keep 1: 2026-09 kept (end = 10-01 > 09-01), 2026-08 pruned.
        assert!(!is_prunable_period(cur - 1, cur, 1));
        assert!(is_prunable_period(cur - 2, cur, 1));
        // keep 3: 2026-07 kept, 2026-06 pruned.
        assert!(!is_prunable_period(cur - 3, cur, 3));
        assert!(is_prunable_period(cur - 4, cur, 3));
        // current and future never.
        assert!(!is_prunable_period(cur, cur, 1));
        assert!(!is_prunable_period(cur + 1, cur, 1));
    }
}
