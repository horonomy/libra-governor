use libra_governor_domain::{
    Admission, CompletionContract, CompletionCriterion, Estimate, ExecutionEvent,
    ExecutionEventKind, ExecutionOutcome, ExecutionPlan, ExecutionReceipt, ExternalRef, PlanId,
    ReplanId, ReplanReason, ReplanRecord, ResourceAmount, TaskFeatures, TaskId, TaskIdentity,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
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

/// Deserializes a nullable `task_features_json` column value into an
/// `Option<TaskFeatures>`. `None` means "genuinely no features recorded"
/// (a pre-HORO-1130 row), not a deserialize failure.
fn parse_task_features(json: Option<String>) -> Result<Option<TaskFeatures>, LedgerError> {
    json.map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
}

/// Raw `plans` row shape for [`LedgerStore::get_plan`]: `(task_id,
/// contract_revision, recon_snapshot_ref, created_at, estimate_json,
/// task_features_json, replaces_plan_id, replan_reason_json,
/// admission_json)`.
type PlanRow = (
    String,
    i64,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Deserializes a nullable `replan_reason_json` column value.
fn parse_replan_reason(json: Option<String>) -> Result<Option<ReplanReason>, LedgerError> {
    json.map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
}

/// Deserializes a nullable `admission_json` column value (HORO-1146).
/// `None` means "genuinely no admission recorded" (a pre-HORO-1146 row,
/// or a plan constructed without evaluating a policy), not a
/// deserialize failure.
fn parse_admission(json: Option<String>) -> Result<Option<Admission>, LedgerError> {
    json.map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
}

/// Parses a nullable `replaces_plan_id` column value into an
/// `Option<PlanId>`.
fn parse_plan_id(id: Option<String>) -> Result<Option<PlanId>, LedgerError> {
    id.map(|s| {
        Uuid::parse_str(&s)
            .map(PlanId)
            .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
    })
    .transpose()
}

/// Deserializes a nullable `reservation_evidence_json` column value
/// (HORO-1141). `None` means "genuinely no reservation evidence
/// recorded" (a pre-HORO-1141 row, or a task with no `task_budgets`
/// row), not a deserialize failure.
fn parse_reservation_evidence(
    json: Option<String>,
) -> Result<Option<libra_governor_domain::ReservationEvidence>, LedgerError> {
    json.map(|s| serde_json::from_str(&s))
        .transpose()
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
            events.push(ExecutionEvent::new(
                task_id,
                parse_time(&occurred_at)?,
                kind,
            ));
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
            "SELECT id, contract_revision, recon_snapshot_ref, created_at, estimate_json, task_features_json, replaces_plan_id, replan_reason_json, admission_json
             FROM plans WHERE task_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([task_id_str], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })?;

        let mut plans = Vec::new();
        for row in rows {
            let (
                id,
                contract_revision,
                recon_snapshot_ref,
                created_at,
                estimate_json,
                task_features_json,
                replaces_plan_id,
                replan_reason_json,
                admission_json,
            ) = row?;
            let id = Uuid::parse_str(&id)
                .map(PlanId)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            let estimate: Option<Estimate> = estimate_json
                .map(|s| serde_json::from_str(&s))
                .transpose()
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            let task_features = parse_task_features(task_features_json)?;
            plans.push(ExecutionPlan {
                id,
                task_id,
                contract_revision: contract_revision as u32,
                recon_snapshot_ref,
                created_at: parse_time(&created_at)?,
                estimate,
                task_features,
                replaces: parse_plan_id(replaces_plan_id)?,
                replan_reason: parse_replan_reason(replan_reason_json)?,
                admission: parse_admission(admission_json)?,
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
                    outcome_json, recorded_at, tool_call_count, model, provider, task_features_json,
                    reservation_evidence_json
             FROM receipts WHERE task_id = ?1 ORDER BY recorded_at ASC",
        )?;
        let rows = stmt.query_map([task_id_str], |row| {
            let plan_id: String = row.get(0)?;
            let contract_revision: i64 = row.get(1)?;
            let actual_duration_secs: i64 = row.get(2)?;
            let actual_usage_json: String = row.get(3)?;
            let outcome_json: String = row.get(4)?;
            let recorded_at: String = row.get(5)?;
            let tool_call_count: i64 = row.get(6)?;
            let model: Option<String> = row.get(7)?;
            let provider: Option<String> = row.get(8)?;
            let task_features_json: Option<String> = row.get(9)?;
            let reservation_evidence_json: Option<String> = row.get(10)?;
            Ok((
                plan_id,
                contract_revision,
                actual_duration_secs,
                actual_usage_json,
                outcome_json,
                recorded_at,
                tool_call_count,
                model,
                provider,
                task_features_json,
                reservation_evidence_json,
            ))
        })?;

        let mut receipts = Vec::new();
        for row in rows {
            let (
                plan_id,
                contract_revision,
                actual_duration_secs,
                usage_json,
                outcome_json,
                recorded_at,
                tool_call_count,
                model,
                provider,
                task_features_json,
                reservation_evidence_json,
            ) = row?;
            let plan_id = Uuid::parse_str(&plan_id)
                .map(PlanId)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            let actual_usage: Vec<ResourceAmount> = serde_json::from_str(&usage_json)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            let outcome: ExecutionOutcome = serde_json::from_str(&outcome_json)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            let task_features = parse_task_features(task_features_json)?;
            let reservations = parse_reservation_evidence(reservation_evidence_json)?;

            receipts.push(ExecutionReceipt {
                task_id,
                contract_revision: contract_revision as u32,
                plan_id,
                actual_duration_secs: actual_duration_secs as u64,
                actual_usage,
                outcome,
                recorded_at: parse_time(&recorded_at)?,
                tool_call_count: tool_call_count as u64,
                model,
                provider,
                task_features,
                reservations,
            });
        }
        Ok(receipts)
    }

    /// Returns the single [`ExecutionPlan`] with `plan_id`, if any.
    pub fn get_plan(&self, plan_id: PlanId) -> Result<Option<ExecutionPlan>, LedgerError> {
        let plan_id_str = plan_id.0.to_string();
        let row: Option<PlanRow> = self
            .conn
            .query_row(
                "SELECT task_id, contract_revision, recon_snapshot_ref, created_at, estimate_json, task_features_json, replaces_plan_id, replan_reason_json, admission_json
                 FROM plans WHERE id = ?1",
                [&plan_id_str],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                    ))
                },
            )
            .ok();

        let Some((
            task_id,
            contract_revision,
            recon_snapshot_ref,
            created_at,
            estimate_json,
            task_features_json,
            replaces_plan_id,
            replan_reason_json,
            admission_json,
        )) = row
        else {
            return Ok(None);
        };

        let task_id = Uuid::parse_str(&task_id)
            .map(TaskId)
            .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
        let estimate: Option<Estimate> = estimate_json
            .map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
        let task_features = parse_task_features(task_features_json)?;

        Ok(Some(ExecutionPlan {
            id: plan_id,
            task_id,
            contract_revision: contract_revision as u32,
            recon_snapshot_ref,
            created_at: parse_time(&created_at)?,
            estimate,
            task_features,
            replaces: parse_plan_id(replaces_plan_id)?,
            replan_reason: parse_replan_reason(replan_reason_json)?,
            admission: parse_admission(admission_json)?,
        }))
    }

    /// Returns every [`ReplanRecord`] persisted for `task_id`, ordered
    /// oldest-first (HORO-1139) — a task's full replan history, queryable
    /// without reconstructing it from `plans` rows.
    pub fn replan_history_for_task(
        &self,
        task_id: TaskId,
    ) -> Result<Vec<ReplanRecord>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, prior_plan_id, new_plan_id, trigger, detail, remaining_estimate_json, created_at
             FROM replan_events WHERE task_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([task_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;

        let mut records = Vec::new();
        for row in rows {
            let (id, prior_plan_id, new_plan_id, trigger_json, detail, remaining_json, created_at) =
                row?;
            records.push(ReplanRecord {
                id: Uuid::parse_str(&id)
                    .map(ReplanId)
                    .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?,
                task_id,
                prior_plan_id: Uuid::parse_str(&prior_plan_id)
                    .map(PlanId)
                    .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?,
                new_plan_id: Uuid::parse_str(&new_plan_id)
                    .map(PlanId)
                    .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?,
                reason: ReplanReason {
                    trigger: serde_json::from_str(&trigger_json)
                        .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?,
                    detail,
                },
                remaining_estimate: serde_json::from_str(&remaining_json)
                    .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?,
                created_at: parse_time(&created_at)?,
            });
        }
        Ok(records)
    }

    /// Returns the currently `in_flight` plan for `session_id`, if any.
    /// Used by `Finalize` to find the estimate a Stop-hook receipt should
    /// be compared against.
    pub fn in_flight_plan_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<PlanId>, LedgerError> {
        let plan_id: Option<String> = self
            .conn
            .query_row(
                "SELECT plan_id FROM session_preflights
                 WHERE session_id = ?1 AND status = 'in_flight'
                 ORDER BY created_at DESC LIMIT 1",
                [session_id],
                |row| row.get(0),
            )
            .ok();
        plan_id
            .map(|s| {
                Uuid::parse_str(&s)
                    .map(PlanId)
                    .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
            })
            .transpose()
    }

    /// Returns every locally recorded [`ExecutionReceipt`], across all
    /// tasks, paired with the [`TaskFeatures`] its originating plan was
    /// estimated against (`None` for a pre-HORO-1130 receipt) — the
    /// sample set `libra-governor-estimator::estimate_bucketed` buckets
    /// and computes quantiles from.
    ///
    /// Previously (HORO-1126) this took an always-ignored `_task_class:
    /// Option<&str>` parameter: nothing in the schema classified tasks
    /// yet, so it was dead code by design. HORO-1130 activates real
    /// bucketing, so the parameter is replaced by the richer return type
    /// the estimator's ladder actually needs, rather than resurrecting an
    /// unused string-classifier parameter no caller ever had a value for.
    pub fn receipts_for_estimation(
        &self,
    ) -> Result<Vec<(Option<TaskFeatures>, ExecutionReceipt)>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT task_id, plan_id, contract_revision, actual_duration_secs,
                    actual_usage_json, outcome_json, recorded_at, tool_call_count, model, provider,
                    task_features_json, reservation_evidence_json
             FROM receipts ORDER BY recorded_at ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let task_id: String = row.get(0)?;
            let plan_id: String = row.get(1)?;
            let contract_revision: i64 = row.get(2)?;
            let actual_duration_secs: i64 = row.get(3)?;
            let actual_usage_json: String = row.get(4)?;
            let outcome_json: String = row.get(5)?;
            let recorded_at: String = row.get(6)?;
            let tool_call_count: i64 = row.get(7)?;
            let model: Option<String> = row.get(8)?;
            let provider: Option<String> = row.get(9)?;
            let task_features_json: Option<String> = row.get(10)?;
            let reservation_evidence_json: Option<String> = row.get(11)?;
            Ok((
                task_id,
                plan_id,
                contract_revision,
                actual_duration_secs,
                actual_usage_json,
                outcome_json,
                recorded_at,
                tool_call_count,
                model,
                provider,
                task_features_json,
                reservation_evidence_json,
            ))
        })?;

        let mut receipts = Vec::new();
        for row in rows {
            let (
                task_id,
                plan_id,
                contract_revision,
                actual_duration_secs,
                usage_json,
                outcome_json,
                recorded_at,
                tool_call_count,
                model,
                provider,
                task_features_json,
                reservation_evidence_json,
            ) = row?;
            let task_id = Uuid::parse_str(&task_id)
                .map(TaskId)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            let plan_id = Uuid::parse_str(&plan_id)
                .map(PlanId)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            let actual_usage: Vec<ResourceAmount> = serde_json::from_str(&usage_json)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            let outcome: ExecutionOutcome = serde_json::from_str(&outcome_json)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            let task_features = parse_task_features(task_features_json)?;
            let reservations = parse_reservation_evidence(reservation_evidence_json)?;

            receipts.push((
                task_features.clone(),
                ExecutionReceipt {
                    task_id,
                    contract_revision: contract_revision as u32,
                    plan_id,
                    actual_duration_secs: actual_duration_secs as u64,
                    actual_usage,
                    outcome,
                    recorded_at: parse_time(&recorded_at)?,
                    tool_call_count: tool_call_count as u64,
                    model,
                    provider,
                    task_features,
                    reservations,
                },
            ));
        }
        Ok(receipts)
    }

    /// Joins every locally recorded [`ExecutionReceipt`] back to the
    /// [`Estimate`] its originating plan was actually made from — the
    /// "receipt vs. its own estimate" pairing nothing before HORO-1132
    /// computed. Rows are dropped (never fabricated) when the plan they
    /// point at carries no estimate at all (a plan predating HORO-1126,
    /// impossible in a fresh MVP 2.0 database but not in one upgraded in
    /// place) or when the estimate is a cold-start one: a cold-start
    /// estimate has no real bounds to be "inside" of, so including it
    /// would be measuring nothing — the exact failure mode that made the
    /// HORO-1127 validation corpus unusable as calibration evidence.
    ///
    /// Returns the kept pairs (ordered by `recorded_at` ascending) plus
    /// the count of dropped rows, so the caller can surface that count
    /// rather than silently discarding it.
    pub fn calibration_pairs(&self) -> Result<(Vec<CalibrationPair>, usize), LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT r.actual_duration_secs, r.recorded_at, r.task_features_json, p.estimate_json
             FROM receipts r JOIN plans p ON r.plan_id = p.id
             ORDER BY r.recorded_at ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let actual_duration_secs: i64 = row.get(0)?;
            let recorded_at: String = row.get(1)?;
            let task_features_json: Option<String> = row.get(2)?;
            let estimate_json: Option<String> = row.get(3)?;
            Ok((
                actual_duration_secs,
                recorded_at,
                task_features_json,
                estimate_json,
            ))
        })?;

        let mut pairs = Vec::new();
        let mut dropped = 0usize;
        for row in rows {
            let (actual_duration_secs, recorded_at, task_features_json, estimate_json) = row?;

            let Some(estimate_json) = estimate_json else {
                dropped += 1;
                continue;
            };
            let estimate: Estimate = serde_json::from_str(&estimate_json)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            if estimate.cold_start {
                dropped += 1;
                continue;
            }

            let task_features = parse_task_features(task_features_json)?;

            pairs.push(CalibrationPair {
                estimate,
                actual_duration_secs: actual_duration_secs as u64,
                task_features,
                recorded_at: parse_time(&recorded_at)?,
            });
        }
        Ok((pairs, dropped))
    }

    /// Coarse, privacy-safe aggregate counts over this ledger's entire
    /// history (HORO-1154's `evidence-report` CLI command). Every number
    /// here is a `COUNT(*)`-style aggregate or a parsed
    /// [`Admission`]/[`ReplanRecord`] variant tag — never a task
    /// description, prompt fragment, tool argument, or any other content
    /// field. See `crates/cli/src/evidence_report_cmd.rs` for the caller
    /// and its own no-network-call, opt-in-gated contract.
    pub fn evidence_aggregates(&self) -> Result<EvidenceAggregates, LedgerError> {
        let task_count: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))?;

        // Every `plans` row with no `replaces_plan_id` is an original
        // preflight plan (HORO-1146's `plan_admission` migration doc);
        // one with a `replaces_plan_id` is a replan-produced plan,
        // counted separately below via `replan_events` (the durable,
        // purpose-built replan history table).
        let preflight_count: u64 = self.conn.query_row(
            "SELECT COUNT(*) FROM plans WHERE replaces_plan_id IS NULL",
            [],
            |row| row.get(0),
        )?;
        let replan_count: u64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM replan_events", [], |row| row.get(0))?;

        let completed_task_count: u64 =
            self.conn
                .query_row("SELECT COUNT(DISTINCT task_id) FROM receipts", [], |row| {
                    row.get(0)
                })?;
        let completed_execution_count: u64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM receipts", [], |row| row.get(0))?;

        let mut admission_admit_count = 0u64;
        let mut admission_deny_count = 0u64;
        let mut admission_approval_required_count = 0u64;
        let mut admission_unrecorded_count = 0u64;
        {
            let mut stmt = self
                .conn
                .prepare("SELECT admission_json FROM plans WHERE replaces_plan_id IS NULL")?;
            let rows = stmt.query_map([], |row| row.get::<_, Option<String>>(0))?;
            for row in rows {
                match parse_admission(row?)? {
                    None => admission_unrecorded_count += 1,
                    Some(Admission::Admit) => admission_admit_count += 1,
                    Some(Admission::Deny(_)) => admission_deny_count += 1,
                    Some(Admission::ApprovalRequired(_)) => admission_approval_required_count += 1,
                }
            }
        }

        let earliest_task_created_at: Option<String> =
            self.conn
                .query_row("SELECT MIN(created_at) FROM tasks", [], |row| row.get(0))?;

        Ok(EvidenceAggregates {
            task_count,
            preflight_count,
            replan_count,
            completed_task_count,
            completed_execution_count,
            admission_admit_count,
            admission_deny_count,
            admission_approval_required_count,
            admission_unrecorded_count,
            earliest_task_created_at,
        })
    }

    /// One row per original (non-replan) preflight plan that carries a
    /// `dogfood_event_id` (HORO-1376) — the DogFood evidence adapter's
    /// (`crates/evidence-adapter`) read-only source for plan/admission
    /// evidence records. Rows with `dogfood_event_id IS NULL` (written
    /// before migration 0010 existed) are skipped entirely rather than
    /// backfilled with a fabricated id — see that migration's header.
    ///
    /// `has_receipt` is computed via a correlated lookup against
    /// `receipts.plan_id` so the adapter can distinguish "admission
    /// verdict recorded, work actually executed" from "recorded, never
    /// executed" without a second query per plan.
    pub fn dogfood_plan_records(&self) -> Result<Vec<DogfoodPlanRecord>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT p.dogfood_event_id, p.task_id, p.created_at, p.dogfood_ingested_at,
                    p.dogfood_origin_profile, p.admission_json,
                    EXISTS(SELECT 1 FROM receipts r WHERE r.plan_id = p.id) AS has_receipt
             FROM plans p
             WHERE p.replaces_plan_id IS NULL AND p.dogfood_event_id IS NOT NULL
             ORDER BY p.created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, bool>(6)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (
                event_id,
                task_id,
                created_at,
                ingested_at,
                origin_profile,
                admission_json,
                has_receipt,
            ) = row?;
            out.push(DogfoodPlanRecord {
                event_id,
                task_id,
                occurred_at: parse_time(&created_at)?,
                ingested_at: ingested_at.map(|s| parse_time(&s)).transpose()?,
                origin_profile,
                admission: parse_admission(admission_json)?,
                has_receipt,
            });
        }
        Ok(out)
    }

    /// One row per execution receipt that carries a `dogfood_event_id`
    /// (HORO-1376) — the DogFood evidence adapter's read-only source for
    /// non-replayable-operation evidence records (ADR-0012 §7.1). Rows
    /// with `dogfood_event_id IS NULL` are skipped for the same reason as
    /// [`Self::dogfood_plan_records`].
    pub fn dogfood_receipt_records(&self) -> Result<Vec<DogfoodReceiptRecord>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT dogfood_event_id, task_id, recorded_at, dogfood_ingested_at,
                    dogfood_origin_profile, actual_usage_json
             FROM receipts
             WHERE dogfood_event_id IS NOT NULL
             ORDER BY recorded_at ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (event_id, task_id, recorded_at, ingested_at, origin_profile, actual_usage_json) =
                row?;
            let actual_usage: Vec<ResourceAmount> = serde_json::from_str(&actual_usage_json)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
            out.push(DogfoodReceiptRecord {
                event_id,
                task_id,
                occurred_at: parse_time(&recorded_at)?,
                ingested_at: ingested_at.map(|s| parse_time(&s)).transpose()?,
                origin_profile,
                actual_usage,
            });
        }
        Ok(out)
    }
}

/// One preflight/admission record read back for the DogFood evidence
/// adapter (HORO-1376). Every field here already exists in the ledger's
/// native schema or is a capture-time stamp added by migration 0010 —
/// this struct adds no new source of truth, it only names the subset the
/// adapter needs so it does not have to depend on `plans`' full row
/// shape or re-implement [`parse_admission`].
#[derive(Debug, Clone, PartialEq)]
pub struct DogfoodPlanRecord {
    /// Stable, capture-time-generated identity (ADR-0012 §3 `event_id`).
    pub event_id: String,
    pub task_id: String,
    /// The plan's own `created_at` — ADR-0012 §3 `occurred_at`.
    pub occurred_at: time::OffsetDateTime,
    /// Capture-time stamp, written strictly after the row's durable
    /// insert returned — ADR-0012 §3 `ingested_at`. `None` only for a
    /// pre-migration-0010 row that somehow also carries a
    /// `dogfood_event_id` (should not occur in practice; the adapter
    /// treats it the same as any other missing timestamp).
    pub ingested_at: Option<time::OffsetDateTime>,
    /// Profile captured at insert time — ADR-0012 §3 `origin_profile`.
    /// `None` for a row written before this stamp existed.
    pub origin_profile: Option<String>,
    pub admission: Option<Admission>,
    /// Whether a receipt exists for this plan's id — used to derive
    /// `actual_action` without ever inferring `deny`/`warn` as something
    /// that "actually happened" (see `evidence-adapter::adapter` docs).
    pub has_receipt: bool,
}

/// One execution receipt record read back for the DogFood evidence
/// adapter (HORO-1376) — see [`DogfoodPlanRecord`] docs for the general
/// shape rationale.
#[derive(Debug, Clone, PartialEq)]
pub struct DogfoodReceiptRecord {
    pub event_id: String,
    pub task_id: String,
    /// The receipt's own `recorded_at` — ADR-0012 §3 `occurred_at`.
    pub occurred_at: time::OffsetDateTime,
    pub ingested_at: Option<time::OffsetDateTime>,
    pub origin_profile: Option<String>,
    /// Verbatim `actual_usage` — never re-derived or estimated from
    /// `tool_call_count` (see the adapter's non-fabrication test).
    pub actual_usage: Vec<ResourceAmount>,
}

/// Coarse, privacy-safe aggregate counts returned by
/// [`LedgerStore::evidence_aggregates`]. Every field is a count, a
/// timestamp, or an admission-outcome tally — deliberately shaped so it
/// is structurally impossible for this type to carry a prompt fragment,
/// file path, or tool-output string.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EvidenceAggregates {
    /// Total distinct tasks this ledger has ever recorded.
    pub task_count: u64,
    /// Original (non-replan) preflight plans — one per admission attempt.
    pub preflight_count: u64,
    /// Replan events recorded in `replan_events` (auto or material-event
    /// triggered), across all tasks.
    pub replan_count: u64,
    /// Distinct tasks with at least one finalized Execution Receipt.
    pub completed_task_count: u64,
    /// Total Execution Receipts recorded (a task can in principle gain
    /// more than one plan/receipt pair across replans and restarts, so
    /// this can exceed `completed_task_count`).
    pub completed_execution_count: u64,
    /// Preflight (non-replan) plans whose recorded admission was `Admit`.
    pub admission_admit_count: u64,
    /// Preflight (non-replan) plans whose recorded admission was `Deny`.
    pub admission_deny_count: u64,
    /// Preflight (non-replan) plans whose recorded admission was
    /// `ApprovalRequired`.
    pub admission_approval_required_count: u64,
    /// Preflight (non-replan) plans with no recorded admission at all —
    /// a pre-HORO-1146 row, or a plan constructed without evaluating a
    /// policy. Reported explicitly rather than silently folded into one
    /// of the other buckets.
    pub admission_unrecorded_count: u64,
    /// RFC 3339 timestamp of this ledger's earliest recorded task, if
    /// any — the closest honest proxy this ledger has for "install
    /// date" (the ledger file's own creation time is a filesystem
    /// property `evidence-report` reports separately, not from here).
    pub earliest_task_created_at: Option<String>,
}

/// One finalized [`ExecutionReceipt`] paired with the [`Estimate`] its
/// originating plan was actually made from — the sample set
/// `libra-governor-estimator::calibration` computes duration coverage and
/// admission-replay metrics from. See
/// [`LedgerStore::calibration_pairs`].
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationPair {
    pub estimate: Estimate,
    pub actual_duration_secs: u64,
    /// The features the finalized receipt carried (HORO-1130), if any —
    /// `None` for a pre-MVP-2 receipt. Kept alongside the estimate so a
    /// caller can stratify by task class without a second query.
    pub task_features: Option<TaskFeatures>,
    pub recorded_at: OffsetDateTime,
}

#[cfg(test)]
mod calibration_pairs_tests {
    use super::*;
    use libra_governor_domain::{BucketTier, CompletionCriterion, Confidence, TaskIdentity};

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    fn computed_estimate() -> Estimate {
        Estimate {
            duration_p50_secs: Some(30),
            duration_p80_secs: Some(50),
            duration_p90_secs: Some(60),
            resource_p50: None,
            resource_p80: None,
            resource_p90: None,
            confidence: Confidence::Medium,
            sample_count: 8,
            cold_start: false,
            estimator_version: "v3-tiered-confidence".to_string(),
            reason: None,
            feature_schema_version: "fs-v1".to_string(),
            bucket_tier: BucketTier::Global,
        }
    }

    /// Inserts one full task -> contract -> plan -> receipt trajectory so
    /// a `calibration_pairs()` row exists for it. `estimate` is attached
    /// to the plan verbatim (`None` simulates a pre-HORO-1126 plan).
    fn seed_receipt(store: &mut LedgerStore, estimate: Option<Estimate>, actual_secs: u64) {
        let identity = TaskIdentity::new(None);
        store.insert_task(&identity, now()).unwrap();
        let contract = CompletionContract::first(vec![CompletionCriterion::required("c")]);
        store
            .insert_contract(identity.id, &contract, now())
            .unwrap();

        let mut plan = ExecutionPlan::new(identity.id, contract.revision, None, now());
        if let Some(estimate) = estimate {
            plan = plan.with_estimate(estimate);
        }
        store.insert_plan(&plan).unwrap();

        let receipt = ExecutionReceipt::new(
            identity.id,
            contract.revision,
            plan.id,
            actual_secs,
            vec![],
            ExecutionOutcome::Unknown,
            now(),
        );
        store.insert_receipt(&receipt).unwrap();
    }

    #[test]
    fn excludes_receipts_whose_plan_has_no_estimate_at_all() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        seed_receipt(&mut store, None, 42);

        let (pairs, dropped) = store.calibration_pairs().unwrap();
        assert!(pairs.is_empty());
        assert_eq!(dropped, 1);
    }

    #[test]
    fn excludes_cold_start_estimates() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        seed_receipt(&mut store, Some(Estimate::cold_start()), 42);

        let (pairs, dropped) = store.calibration_pairs().unwrap();
        assert!(pairs.is_empty());
        assert_eq!(dropped, 1);
    }

    #[test]
    fn keeps_a_real_computed_estimate_paired_with_its_actual() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        seed_receipt(&mut store, Some(computed_estimate()), 55);

        let (pairs, dropped) = store.calibration_pairs().unwrap();
        assert_eq!(dropped, 0);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].actual_duration_secs, 55);
        assert_eq!(pairs[0].estimate.duration_p90_secs, Some(60));
        assert!(!pairs[0].estimate.cold_start);
    }

    #[test]
    fn a_mix_of_rows_keeps_only_the_eligible_ones() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        seed_receipt(&mut store, Some(computed_estimate()), 10);
        seed_receipt(&mut store, Some(computed_estimate()), 20);
        seed_receipt(&mut store, Some(Estimate::cold_start()), 30);
        seed_receipt(&mut store, None, 40);

        let (pairs, dropped) = store.calibration_pairs().unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(dropped, 2);
    }
}

#[cfg(test)]
mod evidence_aggregates_tests {
    use super::*;
    use libra_governor_domain::{
        BucketTier, CompletionCriterion, Confidence, DenyReason, ReplanTriggerKind, TaskIdentity,
    };

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    /// Seeds one task with a contract and a preflight plan carrying the
    /// given admission (or none), returning the task id and plan.
    fn seed_preflight(store: &mut LedgerStore, admission: Option<Admission>) -> (TaskId, PlanId) {
        let identity = TaskIdentity::new(None);
        store.insert_task(&identity, now()).unwrap();
        let contract = CompletionContract::first(vec![CompletionCriterion::required("c")]);
        store
            .insert_contract(identity.id, &contract, now())
            .unwrap();
        let mut plan = ExecutionPlan::new(identity.id, contract.revision, None, now());
        if let Some(admission) = admission {
            plan = plan.with_admission(admission);
        }
        let plan_id = plan.id;
        store.insert_plan(&plan).unwrap();
        (identity.id, plan_id)
    }

    #[test]
    fn an_empty_ledger_reports_all_zero_counts_and_no_earliest_task() {
        let store = LedgerStore::open_in_memory().unwrap();
        let agg = store.evidence_aggregates().unwrap();
        assert_eq!(agg, EvidenceAggregates::default());
    }

    #[test]
    fn aggregates_reflect_real_seeded_state_exactly() {
        let mut store = LedgerStore::open_in_memory().unwrap();

        // Two admitted preflights, one denied, one with no admission
        // recorded at all.
        seed_preflight(&mut store, Some(Admission::Admit));
        let (task_id, plan_id) = seed_preflight(&mut store, Some(Admission::Admit));
        seed_preflight(
            &mut store,
            Some(Admission::Deny(vec![
                DenyReason::ConfidenceBelowThreshold {
                    actual: Confidence::Low,
                    required: Confidence::Medium,
                },
            ])),
        );
        seed_preflight(&mut store, None);

        // One replan against the second admitted task.
        let reason = ReplanReason::new(ReplanTriggerKind::ToolCallCountExceeded, "test replan");
        let remaining = libra_governor_domain::RemainingEstimate::from_bucketed(Estimate {
            duration_p50_secs: Some(30),
            duration_p80_secs: Some(50),
            duration_p90_secs: Some(60),
            resource_p50: None,
            resource_p80: None,
            resource_p90: None,
            confidence: Confidence::Medium,
            sample_count: 8,
            cold_start: false,
            estimator_version: "v3-tiered-confidence".to_string(),
            reason: None,
            feature_schema_version: "fs-v1".to_string(),
            bucket_tier: BucketTier::Global,
        });
        let new_plan = ExecutionPlan::new(task_id, 1, None, now())
            .with_replan_linkage(plan_id, reason.clone());
        store.insert_plan(&new_plan).unwrap();
        let record = ReplanRecord {
            id: ReplanId::new(),
            task_id,
            prior_plan_id: plan_id,
            new_plan_id: new_plan.id,
            reason,
            remaining_estimate: remaining,
            created_at: now(),
        };
        store.insert_replan_event(&record).unwrap();

        // One finalized receipt for that same task.
        let receipt = ExecutionReceipt::new(
            task_id,
            1,
            new_plan.id,
            120,
            vec![],
            ExecutionOutcome::Unknown,
            now(),
        );
        store.insert_receipt(&receipt).unwrap();

        let agg = store.evidence_aggregates().unwrap();
        assert_eq!(agg.task_count, 4);
        assert_eq!(agg.preflight_count, 4);
        assert_eq!(agg.replan_count, 1);
        assert_eq!(agg.completed_task_count, 1);
        assert_eq!(agg.completed_execution_count, 1);
        assert_eq!(agg.admission_admit_count, 2);
        assert_eq!(agg.admission_deny_count, 1);
        assert_eq!(agg.admission_approval_required_count, 0);
        assert_eq!(agg.admission_unrecorded_count, 1);
        assert!(agg.earliest_task_created_at.is_some());
    }
}

#[cfg(test)]
mod replan_persistence_tests {
    use super::*;
    use libra_governor_domain::{
        CompletionCriterion, Confidence, RemainingEstimate, ReplanTriggerKind, TaskIdentity,
    };

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    fn seed_plan(store: &mut LedgerStore) -> (TaskId, ExecutionPlan) {
        let identity = TaskIdentity::new(None);
        store.insert_task(&identity, now()).unwrap();
        let contract = CompletionContract::first(vec![CompletionCriterion::required("c")]);
        store
            .insert_contract(identity.id, &contract, now())
            .unwrap();
        let plan = ExecutionPlan::new(identity.id, contract.revision, None, now());
        store.insert_plan(&plan).unwrap();
        (identity.id, plan)
    }

    fn sample_remaining_estimate() -> RemainingEstimate {
        RemainingEstimate::from_bucketed(Estimate {
            duration_p50_secs: Some(30),
            duration_p80_secs: Some(50),
            duration_p90_secs: Some(60),
            resource_p50: None,
            resource_p80: None,
            resource_p90: None,
            confidence: Confidence::Medium,
            sample_count: 8,
            cold_start: false,
            estimator_version: "v3-tiered-confidence".to_string(),
            reason: None,
            feature_schema_version: "fs-v1".to_string(),
            bucket_tier: libra_governor_domain::BucketTier::Global,
        })
    }

    #[test]
    fn a_replanned_plan_persists_its_linkage_and_round_trips() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let (task_id, prior) = seed_plan(&mut store);

        let reason = ReplanReason::new(
            ReplanTriggerKind::ToolCallCountExceeded,
            "n=17 vs typical 6",
        );
        let new_plan = ExecutionPlan::new(task_id, prior.contract_revision, None, now())
            .with_estimate(sample_remaining_estimate().estimate.clone())
            .with_replan_linkage(prior.id, reason.clone());
        store.insert_plan(&new_plan).unwrap();

        let fetched = store.get_plan(new_plan.id).unwrap().expect("plan exists");
        assert_eq!(fetched.replaces, Some(prior.id));
        assert_eq!(fetched.replan_reason, Some(reason));
    }

    #[test]
    fn an_original_plan_carries_no_replan_linkage_after_round_tripping() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let (_task_id, plan) = seed_plan(&mut store);

        let fetched = store.get_plan(plan.id).unwrap().expect("plan exists");
        assert_eq!(fetched.replaces, None);
        assert_eq!(fetched.replan_reason, None);
    }

    #[test]
    fn replan_history_for_task_is_empty_before_any_replan() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let (task_id, _plan) = seed_plan(&mut store);
        assert_eq!(store.replan_history_for_task(task_id).unwrap(), vec![]);
    }

    #[test]
    fn replan_history_for_task_is_queryable_after_a_replan_event() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let (task_id, prior) = seed_plan(&mut store);

        let reason = ReplanReason::new(ReplanTriggerKind::PossibleToolLoop, "4x Bash in a row");
        let new_plan = ExecutionPlan::new(task_id, prior.contract_revision, None, now())
            .with_replan_linkage(prior.id, reason.clone());
        store.insert_plan(&new_plan).unwrap();

        let record = libra_governor_domain::ReplanRecord {
            id: libra_governor_domain::ReplanId::new(),
            task_id,
            prior_plan_id: prior.id,
            new_plan_id: new_plan.id,
            reason: reason.clone(),
            remaining_estimate: sample_remaining_estimate(),
            created_at: now(),
        };
        store.insert_replan_event(&record).unwrap();

        let history = store.replan_history_for_task(task_id).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].prior_plan_id, prior.id);
        assert_eq!(history[0].new_plan_id, new_plan.id);
        assert_eq!(
            history[0].reason.trigger,
            ReplanTriggerKind::PossibleToolLoop
        );
        assert_eq!(history[0].reason.detail, reason.detail);
    }
}
