use libra_governor_domain::{
    CompletionContract, ExecutionEvent, ExecutionPlan, ExecutionReceipt, TaskIdentity,
};
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::{store::LedgerStore, LedgerError};

fn rfc3339(t: time::OffsetDateTime) -> Result<String, LedgerError> {
    t.format(&Rfc3339)
        .map_err(|e| LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))
}

impl LedgerStore {
    /// Inserts a new [`TaskIdentity`]. The durable anchor multiple
    /// sessions' events attach to via `task_id`.
    pub fn insert_task(
        &mut self,
        identity: &TaskIdentity,
        created_at: time::OffsetDateTime,
    ) -> Result<(), LedgerError> {
        let (ref_kind, ref_value) = match &identity.external_ref {
            Some(libra_governor_domain::ExternalRef::Jira(v)) => (Some("jira"), Some(v.clone())),
            Some(libra_governor_domain::ExternalRef::GitHub(v)) => {
                (Some("github"), Some(v.clone()))
            }
            Some(libra_governor_domain::ExternalRef::Other(v)) => (Some("other"), Some(v.clone())),
            None => (None, None),
        };

        self.conn.execute(
            "INSERT INTO tasks (task_id, external_ref_kind, external_ref_value, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                identity.id.to_string(),
                ref_kind,
                ref_value,
                rfc3339(created_at)?
            ],
        )?;
        Ok(())
    }

    /// Inserts a new contract revision (and its criteria) for a task, in a
    /// single transaction. Revisions are append-only: this never mutates
    /// an existing row.
    pub fn insert_contract(
        &mut self,
        task_id: libra_governor_domain::TaskId,
        contract: &CompletionContract,
        created_at: time::OffsetDateTime,
    ) -> Result<(), LedgerError> {
        let created_at = rfc3339(created_at)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO contracts (task_id, revision, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![task_id.to_string(), contract.revision, created_at],
        )?;
        for (ordinal, criterion) in contract.criteria.iter().enumerate() {
            tx.execute(
                "INSERT INTO contract_criteria (task_id, revision, ordinal, description, required)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    task_id.to_string(),
                    contract.revision,
                    ordinal as i64,
                    criterion.description,
                    criterion.required,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Inserts an execution event and updates the owning task's
    /// `last_event_at` in the same transaction.
    ///
    /// `event_id` is the event's idempotency key: re-inserting the same
    /// `event_id` is a no-op (`INSERT OR IGNORE`), so a hook that retries
    /// after an ambiguous failure cannot double-count an event.
    pub fn insert_event(
        &mut self,
        event_id: Uuid,
        event: &ExecutionEvent,
    ) -> Result<(), LedgerError> {
        let occurred_at = rfc3339(event.occurred_at)?;
        let payload_json = serde_json::to_string(&event.kind).map_err(|e| {
            LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;

        let tx = self.conn.transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO events (id, task_id, occurred_at, payload_json)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                event_id.to_string(),
                event.task_id.to_string(),
                occurred_at,
                payload_json,
            ],
        )?;
        if inserted > 0 {
            tx.execute(
                "UPDATE tasks SET last_event_at = ?1 WHERE task_id = ?2",
                rusqlite::params![occurred_at, event.task_id.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Inserts an [`ExecutionPlan`], linking it to the task and the exact
    /// contract revision it assumed. `plan.estimate`, if present, is
    /// stored alongside so a later `Finalize` request can compare against
    /// the ORIGINAL estimate without re-running the estimator (HORO-1126).
    pub fn insert_plan(&mut self, plan: &ExecutionPlan) -> Result<(), LedgerError> {
        let estimate_json = plan
            .estimate
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| {
                LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            })?;

        self.conn.execute(
            "INSERT INTO plans (id, task_id, contract_revision, recon_snapshot_ref, created_at, estimate_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                plan.id.0.to_string(),
                plan.task_id.to_string(),
                plan.contract_revision,
                plan.recon_snapshot_ref,
                rfc3339(plan.created_at)?,
                estimate_json,
            ],
        )?;
        Ok(())
    }

    /// Inserts an [`ExecutionReceipt`], the estimate-vs-actual record for
    /// one plan.
    pub fn insert_receipt(&mut self, receipt: &ExecutionReceipt) -> Result<(), LedgerError> {
        let usage_json = serde_json::to_string(&receipt.actual_usage).map_err(|e| {
            LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        let outcome_json = serde_json::to_string(&receipt.outcome).map_err(|e| {
            LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;

        self.conn.execute(
            "INSERT INTO receipts (task_id, plan_id, contract_revision, actual_duration_secs,
                                    actual_usage_json, outcome_json, recorded_at,
                                    tool_call_count, model, provider)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                receipt.task_id.to_string(),
                receipt.plan_id.0.to_string(),
                receipt.contract_revision,
                receipt.actual_duration_secs as i64,
                usage_json,
                outcome_json,
                rfc3339(receipt.recorded_at)?,
                receipt.tool_call_count as i64,
                receipt.model,
                receipt.provider,
            ],
        )?;
        Ok(())
    }
}
