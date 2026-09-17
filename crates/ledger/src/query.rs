use libra_governor_domain::{
    CompletionContract, CompletionCriterion, ExecutionEvent, ExecutionEventKind, ExecutionOutcome,
    ExecutionPlan, ExecutionReceipt, ExternalRef, PlanId, ResourceAmount, TaskId, TaskIdentity,
};
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::{store::LedgerStore, LedgerError};

/// One task's full trajectory, reconstructed purely from SQLite: its
/// identity, every recorded event (in order), every contract revision,
/// every plan, and every receipt. This is the acceptance-critical
/// "receipt query" — it never needs to parse a chat transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskTrajectory {
    pub task: TaskIdentity,
    pub events: Vec<ExecutionEvent>,
    pub contracts: Vec<CompletionContract>,
    pub plans: Vec<ExecutionPlan>,
    pub receipts: Vec<ExecutionReceipt>,
}

fn parse_time(s: &str) -> Result<time::OffsetDateTime, LedgerError> {
    time::OffsetDateTime::parse(s, &Rfc3339)
        .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
}

impl LedgerStore {
    /// Reconstructs one task's full trajectory: identity, ordered events,
    /// all contract revisions, all plans, and all receipts.
    pub fn task_trajectory(&self, task_id: TaskId) -> Result<TaskTrajectory, LedgerError> {
        let task_id_str = task_id.to_string();

        let task = self
            .conn
            .query_row(
                "SELECT external_ref_kind, external_ref_value FROM tasks WHERE task_id = ?1",
                [&task_id_str],
                |row| {
                    let kind: Option<String> = row.get(0)?;
                    let value: Option<String> = row.get(1)?;
                    Ok((kind, value))
                },
            )
            .map_err(|_| LedgerError::TaskNotFound(task_id_str.clone()))?;

        let external_ref = match (task.0.as_deref(), task.1) {
            (Some("jira"), Some(v)) => Some(ExternalRef::Jira(v)),
            (Some("github"), Some(v)) => Some(ExternalRef::GitHub(v)),
            (Some("other"), Some(v)) => Some(ExternalRef::Other(v)),
            _ => None,
        };
        let identity = TaskIdentity {
            id: task_id,
            external_ref,
        };

        let events = self.load_events(&task_id_str)?;
        let contracts = self.load_contracts(&task_id_str)?;
        let plans = self.load_plans(&task_id_str, task_id)?;
        let receipts = self.load_receipts(&task_id_str, task_id)?;

        Ok(TaskTrajectory {
            task: identity,
            events,
            contracts,
            plans,
            receipts,
        })
    }

    fn load_events(&self, task_id_str: &str) -> Result<Vec<ExecutionEvent>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT occurred_at, payload_json FROM events
             WHERE task_id = ?1 ORDER BY occurred_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([task_id_str], |row| {
            let occurred_at: String = row.get(0)?;
            let payload_json: String = row.get(1)?;
            Ok((occurred_at, payload_json))
        })?;

        let task_id = Uuid::parse_str(task_id_str)
            .map(TaskId)
            .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;

        let mut events = Vec::new();
        for row in rows {
            let (occurred_at, payload_json) = row?;
            let kind: ExecutionEventKind = serde_json::from_str(&payload_json)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            events.push(ExecutionEvent::new(task_id, parse_time(&occurred_at)?, kind));
        }
        Ok(events)
    }

    fn load_contracts(&self, task_id_str: &str) -> Result<Vec<CompletionContract>, LedgerError> {
        let mut stmt = self
            .conn
            .prepare("SELECT revision FROM contracts WHERE task_id = ?1 ORDER BY revision ASC")?;
        let revisions: Vec<u32> = stmt
            .query_map([task_id_str], |row| row.get::<_, i64>(0))?
            .map(|r| r.map(|v| v as u32))
            .collect::<Result<_, _>>()?;

        let mut criteria_stmt = self.conn.prepare(
            "SELECT description, required FROM contract_criteria
             WHERE task_id = ?1 AND revision = ?2 ORDER BY ordinal ASC",
        )?;

        let mut contracts = Vec::new();
        for revision in revisions {
            let criteria: Vec<CompletionCriterion> = criteria_stmt
                .query_map(rusqlite::params![task_id_str, revision], |row| {
                    Ok(CompletionCriterion {
                        description: row.get(0)?,
                        required: row.get(1)?,
                    })
                })?
                .collect::<Result<_, _>>()?;
            contracts.push(CompletionContract { revision, criteria });
        }
        Ok(contracts)
    }

    fn load_plans(
        &self,
        task_id_str: &str,
        task_id: TaskId,
    ) -> Result<Vec<ExecutionPlan>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, contract_revision, recon_snapshot_ref, created_at
             FROM plans WHERE task_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([task_id_str], |row| {
            let id: String = row.get(0)?;
            let contract_revision: i64 = row.get(1)?;
            let recon_snapshot_ref: Option<String> = row.get(2)?;
            let created_at: String = row.get(3)?;
            Ok((id, contract_revision, recon_snapshot_ref, created_at))
        })?;

        let mut plans = Vec::new();
        for row in rows {
            let (id, contract_revision, recon_snapshot_ref, created_at) = row?;
            let id = Uuid::parse_str(&id)
                .map(PlanId)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            plans.push(ExecutionPlan {
                id,
                task_id,
                contract_revision: contract_revision as u32,
                recon_snapshot_ref,
                created_at: parse_time(&created_at)?,
            });
        }
        Ok(plans)
    }

    fn load_receipts(
        &self,
        task_id_str: &str,
        task_id: TaskId,
    ) -> Result<Vec<ExecutionReceipt>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT plan_id, contract_revision, actual_duration_secs, actual_usage_json,
                    outcome_json, recorded_at
             FROM receipts WHERE task_id = ?1 ORDER BY recorded_at ASC",
        )?;
        let rows = stmt.query_map([task_id_str], |row| {
            let plan_id: String = row.get(0)?;
            let contract_revision: i64 = row.get(1)?;
            let actual_duration_secs: i64 = row.get(2)?;
            let actual_usage_json: String = row.get(3)?;
            let outcome_json: String = row.get(4)?;
            let recorded_at: String = row.get(5)?;
            Ok((
                plan_id,
                contract_revision,
                actual_duration_secs,
                actual_usage_json,
                outcome_json,
                recorded_at,
            ))
        })?;

        let mut receipts = Vec::new();
        for row in rows {
            let (plan_id, contract_revision, actual_duration_secs, usage_json, outcome_json, recorded_at) =
                row?;
            let plan_id = Uuid::parse_str(&plan_id)
                .map(PlanId)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            let actual_usage: Vec<ResourceAmount> = serde_json::from_str(&usage_json)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            let outcome: ExecutionOutcome = serde_json::from_str(&outcome_json)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;

            receipts.push(ExecutionReceipt {
                task_id,
                contract_revision: contract_revision as u32,
                plan_id,
                actual_duration_secs: actual_duration_secs as u64,
                actual_usage,
                outcome,
                recorded_at: parse_time(&recorded_at)?,
            });
        }
        Ok(receipts)
    }
}
