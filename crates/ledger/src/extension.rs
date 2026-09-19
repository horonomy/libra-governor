//! Local extension points: `business_context`, `outcome_attestations`,
//! `webhook_deliveries` (HORO-1174). See
//! `migrations/0009_extension_points.sql` for the schema and its privacy
//! rationale.
//!
//! # This module does not decide
//!
//! Every write here is a plain record of something already decided
//! elsewhere (`crates/daemon::server` narrows a policy and calls
//! `apply_business_context`/`apply_external_approval`; this module only
//! persists the summary and the verdict). No function here evaluates a
//! `Policy` or reads `task_budgets`.

use libra_governor_domain::{PlanId, TaskId};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{store::LedgerStore, LedgerError};

fn rfc3339(t: OffsetDateTime) -> Result<String, LedgerError> {
    t.format(&Rfc3339)
        .map_err(|e| LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))
}

// ---------------------------------------------------------------------
// business_context
// ---------------------------------------------------------------------

/// One `business_context` row to insert.
#[derive(Debug, Clone)]
pub struct BusinessContextInsert<'a> {
    pub id: &'a str,
    pub task_id: TaskId,
    pub plan_id: Option<PlanId>,
    pub provider_id: &'a str,
    pub schema_version: &'a str,
    pub priority: Option<&'a str>,
    pub deadline: Option<OffsetDateTime>,
    pub cost_center: Option<&'a str>,
    /// Pre-serialized JSON — see `libra-governor-extension::wire` for the
    /// cap (16 entries x 200 chars) enforced before this string is ever
    /// constructed.
    pub advisory_criteria_json: Option<&'a str>,
    pub external_refs_json: Option<&'a str>,
    pub applied: bool,
    pub received_at: OffsetDateTime,
}

impl LedgerStore {
    /// `true` if `task_id` has a `tasks` row. Used by `RecordOutcome`
    /// handling to distinguish a genuinely unknown task from one with no
    /// receipt yet.
    pub fn task_exists(&self, task_id: TaskId) -> Result<bool, LedgerError> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM tasks WHERE task_id = ?1",
                [task_id.to_string()],
                |_| Ok(()),
            )
            .optional_flag())
    }

    /// Records one fetched business context.
    pub fn insert_business_context(
        &mut self,
        insert: BusinessContextInsert<'_>,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO business_context (
                id, task_id, plan_id, provider_id, schema_version, priority, deadline,
                cost_center, advisory_criteria_json, external_refs_json, applied, received_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                insert.id,
                insert.task_id.to_string(),
                insert.plan_id.map(|p| p.0.to_string()),
                insert.provider_id,
                insert.schema_version,
                insert.priority,
                insert.deadline.map(rfc3339).transpose()?,
                insert.cost_center,
                insert.advisory_criteria_json,
                insert.external_refs_json,
                insert.applied,
                rfc3339(insert.received_at)?,
            ],
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------
// outcome_attestations
// ---------------------------------------------------------------------

/// One `outcome_attestations` row to insert.
#[derive(Debug, Clone)]
pub struct OutcomeAttestationInsert<'a> {
    pub id: &'a str,
    pub task_id: TaskId,
    pub plan_id: Option<PlanId>,
    pub source: &'a str,
    pub source_id: Option<&'a str>,
    pub outcome_kind: &'a str,
    pub evidence_json: &'a str,
    pub idempotency_key: &'a str,
    pub authoritative: bool,
    pub attested_at: OffsetDateTime,
}

impl LedgerStore {
    /// Inserts an outcome attestation, deduped on
    /// `UNIQUE(task_id, source_id, idempotency_key)`. Returns `true` if a
    /// new row was written, `false` if this exact `(task_id, source_id,
    /// idempotency_key)` was already recorded (a duplicate push — writes
    /// nothing).
    pub fn insert_outcome_attestation(
        &mut self,
        insert: OutcomeAttestationInsert<'_>,
    ) -> Result<bool, LedgerError> {
        let rows = self.conn.execute(
            "INSERT OR IGNORE INTO outcome_attestations (
                id, task_id, plan_id, source, source_id, outcome_kind, evidence_json,
                idempotency_key, authoritative, attested_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                insert.id,
                insert.task_id.to_string(),
                insert.plan_id.map(|p| p.0.to_string()),
                insert.source,
                insert.source_id,
                insert.outcome_kind,
                insert.evidence_json,
                insert.idempotency_key,
                insert.authoritative,
                rfc3339(insert.attested_at)?,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Promotes the most recent receipt for `task_id` (optionally
    /// narrowed to `plan_id`) to carry `outcome_json`. Returns `true` if a
    /// receipt row was updated, `false` if no matching receipt exists yet
    /// (e.g. an outcome pushed before `Finalize` ever ran for this task).
    ///
    /// This is a materialized-view write: `receipts.outcome_json` becomes
    /// "the latest authoritative attestation's outcome" rather than
    /// "whatever `Finalize` last wrote". Verified inert to
    /// `receipts_for_estimation`/`calibration_pairs` (neither filters or
    /// keys on `outcome_json` — see `crates/ledger/src/query.rs`) — an
    /// outcome push does not retroactively change which receipts feed the
    /// estimator or calibration report.
    pub fn promote_receipt_outcome(
        &mut self,
        task_id: TaskId,
        plan_id: Option<PlanId>,
        outcome_json: &str,
    ) -> Result<bool, LedgerError> {
        let rows = match plan_id {
            Some(plan_id) => self.conn.execute(
                "UPDATE receipts SET outcome_json = ?1 WHERE task_id = ?2 AND plan_id = ?3",
                rusqlite::params![outcome_json, task_id.to_string(), plan_id.0.to_string()],
            )?,
            None => self.conn.execute(
                "UPDATE receipts SET outcome_json = ?1 WHERE task_id = ?2 AND plan_id = (
                    SELECT plan_id FROM receipts WHERE task_id = ?2 ORDER BY recorded_at DESC LIMIT 1
                 )",
                rusqlite::params![outcome_json, task_id.to_string()],
            )?,
        };
        Ok(rows > 0)
    }
}

// ---------------------------------------------------------------------
// webhook_deliveries
// ---------------------------------------------------------------------

/// One `webhook_deliveries` row to enqueue.
#[derive(Debug, Clone)]
pub struct DeliveryEnqueue<'a> {
    pub event_id: &'a str,
    pub event_kind: &'a str,
    pub dedupe_key: &'a str,
    pub task_id: Option<TaskId>,
    /// The exact serialized `EventEnvelope` bytes a retry must resend
    /// unchanged — see `libra-governor-extension::event`.
    pub payload_json: &'a str,
    pub now: OffsetDateTime,
}

/// One row read back off `webhook_deliveries`, ready for a delivery
/// attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedDeliveryRow {
    pub event_id: String,
    pub event_kind: String,
    pub payload_json: String,
    pub attempts: u32,
}

impl LedgerStore {
    /// Enqueues one event for delivery, deduped on
    /// `UNIQUE(event_kind, dedupe_key)`. Returns `true` if newly enqueued,
    /// `false` if this `(event_kind, dedupe_key)` was already queued (a
    /// crash-restart replay of the same `handle_preflight`/
    /// `handle_tool_invoked`/`handle_finalize` call) — writes nothing.
    pub fn enqueue_delivery(&mut self, enqueue: DeliveryEnqueue<'_>) -> Result<bool, LedgerError> {
        let rows = self.conn.execute(
            "INSERT OR IGNORE INTO webhook_deliveries (
                event_id, event_kind, dedupe_key, task_id, payload_json, state, attempts,
                next_attempt_at, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, NULL, ?6)",
            rusqlite::params![
                enqueue.event_id,
                enqueue.event_kind,
                enqueue.dedupe_key,
                enqueue.task_id.map(|t| t.to_string()),
                enqueue.payload_json,
                rfc3339(enqueue.now)?,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Selects up to `limit` deliveries in `pending` state whose
    /// `next_attempt_at` has passed (or was never set — a fresh enqueue).
    /// A short, read-only query: the caller must send the HTTP request
    /// and update state in a **separate** transaction, never while
    /// holding this one open — see
    /// `libra-governor-extension::dispatcher` docs on why a long-held
    /// `BEGIN IMMEDIATE` here would stall the daemon's serial accept
    /// loop.
    pub fn due_deliveries(
        &self,
        now: OffsetDateTime,
        limit: usize,
    ) -> Result<Vec<QueuedDeliveryRow>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT event_id, event_kind, payload_json, attempts FROM webhook_deliveries
             WHERE state = 'pending' AND (next_attempt_at IS NULL OR next_attempt_at <= ?1)
             ORDER BY created_at ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![rfc3339(now)?, limit as i64], |row| {
            let attempts: i64 = row.get(3)?;
            Ok(QueuedDeliveryRow {
                event_id: row.get(0)?,
                event_kind: row.get(1)?,
                payload_json: row.get(2)?,
                attempts: attempts as u32,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(LedgerError::from)
    }

    /// Records a successful delivery: `state = 'delivered'`,
    /// `delivered_at` set, `attempts` incremented.
    pub fn record_delivery_success(
        &mut self,
        event_id: &str,
        now: OffsetDateTime,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "UPDATE webhook_deliveries SET state = 'delivered', delivered_at = ?1,
                attempts = attempts + 1, last_status = ?2, last_error = NULL
             WHERE event_id = ?3",
            rusqlite::params![rfc3339(now)?, 200i64, event_id],
        )?;
        Ok(())
    }

    /// Records a failed delivery attempt. `next_attempt_at: None` with
    /// `abandon: true` marks the row `abandoned` (backoff ladder
    /// exhausted); otherwise the row stays `pending` with the given
    /// `next_attempt_at`.
    #[allow(clippy::too_many_arguments)]
    pub fn record_delivery_failure(
        &mut self,
        event_id: &str,
        next_attempt_at: Option<OffsetDateTime>,
        status: Option<u16>,
        error: &str,
        abandon: bool,
    ) -> Result<(), LedgerError> {
        let state = if abandon { "abandoned" } else { "pending" };
        self.conn.execute(
            "UPDATE webhook_deliveries SET state = ?1, attempts = attempts + 1,
                next_attempt_at = ?2, last_status = ?3, last_error = ?4
             WHERE event_id = ?5",
            rusqlite::params![
                state,
                next_attempt_at.map(rfc3339).transpose()?,
                status.map(|s| s as i64),
                error,
                event_id,
            ],
        )?;
        Ok(())
    }

    /// Count of deliveries still `pending` — feeds `Doctor`'s
    /// `extension_events_pending`.
    pub fn pending_delivery_count(&self) -> Result<u64, LedgerError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM webhook_deliveries WHERE state = 'pending'",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    /// Test/diagnostics-facing: the recorded `state` of one delivery.
    pub fn delivery_state(&self, event_id: &str) -> Result<Option<String>, LedgerError> {
        use rusqlite::OptionalExtension;
        self.conn
            .query_row(
                "SELECT state FROM webhook_deliveries WHERE event_id = ?1",
                [event_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Test/diagnostics-facing: the recorded `attempts` of one delivery.
    pub fn delivery_attempts(&self, event_id: &str) -> Result<Option<u32>, LedgerError> {
        use rusqlite::OptionalExtension;
        let attempts: Option<i64> = self
            .conn
            .query_row(
                "SELECT attempts FROM webhook_deliveries WHERE event_id = ?1",
                [event_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(attempts.map(|a| a as u32))
    }
}

/// Turns a `query_row` result into a presence boolean without leaking the
/// underlying `rusqlite::Error::QueryReturnedNoRows` distinction — used
/// only by [`LedgerStore::task_exists`], where "no rows" and "not found"
/// mean the same thing.
trait OptionalFlag {
    fn optional_flag(self) -> bool;
}

impl<T> OptionalFlag for Result<T, rusqlite::Error> {
    fn optional_flag(self) -> bool {
        match self {
            Ok(_) => true,
            Err(rusqlite::Error::QueryReturnedNoRows) => false,
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::TaskIdentity;

    fn store_with_task() -> (LedgerStore, TaskId) {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let identity = TaskIdentity::new(None);
        let task_id = identity.id;
        store
            .insert_task(&identity, OffsetDateTime::UNIX_EPOCH)
            .unwrap();
        (store, task_id)
    }

    #[test]
    fn task_exists_reports_presence_correctly() {
        let (store, task_id) = store_with_task();
        assert!(store.task_exists(task_id).unwrap());
        assert!(!store.task_exists(TaskId::new()).unwrap());
    }

    #[test]
    fn business_context_round_trips() {
        let (mut store, task_id) = store_with_task();
        store
            .insert_business_context(BusinessContextInsert {
                id: "bc-1",
                task_id,
                plan_id: None,
                provider_id: "example-provider",
                schema_version: "libra.extension.v1",
                priority: Some("high"),
                deadline: Some(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(3600)),
                cost_center: Some("eng-platform"),
                advisory_criteria_json: Some(r#"["check the thing"]"#),
                external_refs_json: None,
                applied: true,
                received_at: OffsetDateTime::UNIX_EPOCH,
            })
            .unwrap();

        let (provider_id, applied): (String, bool) = store
            .conn
            .query_row(
                "SELECT provider_id, applied FROM business_context WHERE id = 'bc-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(provider_id, "example-provider");
        assert!(applied);
    }

    #[test]
    fn outcome_attestation_dedupe_by_task_source_and_idempotency_key() {
        let (mut store, task_id) = store_with_task();
        let insert = || OutcomeAttestationInsert {
            id: "attest-1",
            task_id,
            plan_id: None,
            source: "provider",
            source_id: Some("example-provider"),
            outcome_kind: "completed",
            evidence_json: r#"["https://ci.example.com/1"]"#,
            idempotency_key: "ci-run-1",
            authoritative: true,
            attested_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert!(store.insert_outcome_attestation(insert()).unwrap());
        assert!(
            !store.insert_outcome_attestation(insert()).unwrap(),
            "a duplicate idempotency key must be a no-op"
        );

        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM outcome_attestations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn agent_sourced_attestation_is_recorded_but_promote_is_the_caller_s_choice() {
        // This module never inspects `authoritative` to decide whether to
        // promote -- the daemon decides that before calling
        // `promote_receipt_outcome` at all. This test just proves the row
        // is written regardless of the flag's value.
        let (mut store, task_id) = store_with_task();
        let inserted = store
            .insert_outcome_attestation(OutcomeAttestationInsert {
                id: "attest-agent",
                task_id,
                plan_id: None,
                source: "agent",
                source_id: Some("claude_code"),
                outcome_kind: "completed",
                evidence_json: "[]",
                idempotency_key: "agent-claim-1",
                authoritative: false,
                attested_at: OffsetDateTime::UNIX_EPOCH,
            })
            .unwrap();
        assert!(inserted);
    }

    #[test]
    fn promote_receipt_outcome_updates_the_latest_receipt_for_the_task() {
        use libra_governor_domain::{ExecutionOutcome, ExecutionReceipt, PlanId};

        let (mut store, task_id) = store_with_task();
        store
            .insert_contract(
                task_id,
                &libra_governor_domain::CompletionContract::first(vec![]),
                OffsetDateTime::UNIX_EPOCH,
            )
            .unwrap();
        let plan =
            libra_governor_domain::ExecutionPlan::new(task_id, 1, None, OffsetDateTime::UNIX_EPOCH);
        store.insert_plan(&plan).unwrap();
        let receipt = ExecutionReceipt::new(
            task_id,
            1,
            plan.id,
            60,
            vec![],
            ExecutionOutcome::Unknown,
            OffsetDateTime::UNIX_EPOCH,
        );
        store.insert_receipt(&receipt).unwrap();

        let outcome = ExecutionOutcome::Completed {
            evidence: vec!["https://ci.example.com/1".to_string()],
        };
        let outcome_json = serde_json::to_string(&outcome).unwrap();
        let updated = store
            .promote_receipt_outcome(task_id, None, &outcome_json)
            .unwrap();
        assert!(updated);

        let stored: String = store
            .conn
            .query_row(
                "SELECT outcome_json FROM receipts WHERE task_id = ?1 AND plan_id = ?2",
                rusqlite::params![task_id.to_string(), plan.id.0.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let parsed: ExecutionOutcome = serde_json::from_str(&stored).unwrap();
        assert_eq!(parsed, outcome);

        // A plan_id with no matching receipt reports false, not an error.
        assert!(!store
            .promote_receipt_outcome(task_id, Some(PlanId::new()), &outcome_json)
            .unwrap());
    }

    #[test]
    fn promote_receipt_outcome_on_a_task_with_no_receipt_reports_false() {
        let (mut store, task_id) = store_with_task();
        let outcome_json =
            serde_json::to_string(&libra_governor_domain::ExecutionOutcome::Unknown).unwrap();
        assert!(!store
            .promote_receipt_outcome(task_id, None, &outcome_json)
            .unwrap());
    }

    #[test]
    fn enqueue_delivery_dedupes_by_event_kind_and_dedupe_key() {
        let (mut store, task_id) = store_with_task();
        let enqueue = |event_id: &'static str| DeliveryEnqueue {
            event_id,
            event_kind: "admission",
            dedupe_key: "plan-1",
            task_id: Some(task_id),
            payload_json: r#"{"schema_version":"libra.extension.v1"}"#,
            now: OffsetDateTime::UNIX_EPOCH,
        };
        assert!(store.enqueue_delivery(enqueue("ev-1")).unwrap());
        // A second preflight for the same plan (crash-restart replay)
        // must not double-enqueue, even under a different event_id.
        assert!(!store.enqueue_delivery(enqueue("ev-2")).unwrap());

        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM webhook_deliveries", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn due_deliveries_returns_pending_rows_whose_next_attempt_has_passed() {
        let (mut store, task_id) = store_with_task();
        store
            .enqueue_delivery(DeliveryEnqueue {
                event_id: "ev-due",
                event_kind: "outcome",
                dedupe_key: "task-1:plan-1",
                task_id: Some(task_id),
                payload_json: "{}",
                now: OffsetDateTime::UNIX_EPOCH,
            })
            .unwrap();

        let due = store
            .due_deliveries(OffsetDateTime::UNIX_EPOCH, 10)
            .unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].event_id, "ev-due");
        assert_eq!(due[0].attempts, 0);
    }

    #[test]
    fn record_delivery_success_marks_delivered_and_increments_attempts() {
        let (mut store, task_id) = store_with_task();
        store
            .enqueue_delivery(DeliveryEnqueue {
                event_id: "ev-ok",
                event_kind: "outcome",
                dedupe_key: "task-1:plan-1",
                task_id: Some(task_id),
                payload_json: "{}",
                now: OffsetDateTime::UNIX_EPOCH,
            })
            .unwrap();
        store
            .record_delivery_success("ev-ok", OffsetDateTime::UNIX_EPOCH)
            .unwrap();

        assert_eq!(
            store.delivery_state("ev-ok").unwrap(),
            Some("delivered".to_string())
        );
        assert_eq!(store.delivery_attempts("ev-ok").unwrap(), Some(1));
        assert!(store
            .due_deliveries(OffsetDateTime::UNIX_EPOCH, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn record_delivery_failure_reschedules_or_abandons() {
        let (mut store, task_id) = store_with_task();
        store
            .enqueue_delivery(DeliveryEnqueue {
                event_id: "ev-fail",
                event_kind: "outcome",
                dedupe_key: "task-1:plan-1",
                task_id: Some(task_id),
                payload_json: "{}",
                now: OffsetDateTime::UNIX_EPOCH,
            })
            .unwrap();

        let retry_at = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1);
        store
            .record_delivery_failure("ev-fail", Some(retry_at), Some(500), "server error", false)
            .unwrap();
        assert_eq!(
            store.delivery_state("ev-fail").unwrap(),
            Some("pending".to_string())
        );
        assert_eq!(store.delivery_attempts("ev-fail").unwrap(), Some(1));
        // Not yet due before retry_at.
        assert!(store
            .due_deliveries(OffsetDateTime::UNIX_EPOCH, 10)
            .unwrap()
            .is_empty());
        assert_eq!(store.due_deliveries(retry_at, 10).unwrap().len(), 1);

        store
            .record_delivery_failure("ev-fail", None, Some(500), "gave up", true)
            .unwrap();
        assert_eq!(
            store.delivery_state("ev-fail").unwrap(),
            Some("abandoned".to_string())
        );
        assert!(store.due_deliveries(retry_at, 10).unwrap().is_empty());
    }

    #[test]
    fn pending_delivery_count_reflects_only_pending_rows() {
        let (mut store, task_id) = store_with_task();
        for (id, dedupe) in [("ev-a", "a"), ("ev-b", "b")] {
            store
                .enqueue_delivery(DeliveryEnqueue {
                    event_id: id,
                    event_kind: "outcome",
                    dedupe_key: dedupe,
                    task_id: Some(task_id),
                    payload_json: "{}",
                    now: OffsetDateTime::UNIX_EPOCH,
                })
                .unwrap();
        }
        store
            .record_delivery_success("ev-a", OffsetDateTime::UNIX_EPOCH)
            .unwrap();
        assert_eq!(store.pending_delivery_count().unwrap(), 1);
    }

    #[test]
    fn the_schema_has_no_signature_timestamp_or_nonce_column() {
        let store = LedgerStore::open_in_memory().unwrap();
        let mut stmt = store
            .conn
            .prepare("SELECT name FROM pragma_table_info('webhook_deliveries')")
            .unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for forbidden in ["signature", "hmac", "marker", "secret", "hmac_secret"] {
            assert!(
                !columns.iter().any(|c| c == forbidden),
                "webhook_deliveries must never gain a `{forbidden}` column — signing is \
                 per-attempt, computed fresh, never persisted"
            );
        }
    }
}
