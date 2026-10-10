//! Scoped association reads and writes. Daemon policy owns effect admission;
//! mutation methods require its outer transaction so ownership and effects
//! cannot commit separately.
use crate::{LedgerError, LedgerStore};
use libra_governor_domain::{
    AssociationUnavailable, ExecutionIdentity, ExecutionPosition, ExecutionTarget, PlanId, TaskId,
};
use rusqlite::OptionalExtension;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionLookup {
    Found(ExecutionTarget),
    Unavailable(AssociationUnavailable),
}

fn invalid() -> LedgerError {
    LedgerError::Sqlite(rusqlite::Error::InvalidQuery)
}
fn encode<T: serde::Serialize>(value: &T) -> Result<String, LedgerError> {
    serde_json::to_string(value).map_err(|_| invalid())
}

impl LedgerStore {
    pub fn execution_current_turn(
        &self,
        position: &ExecutionPosition,
    ) -> Result<Option<String>, LedgerError> {
        Ok(self
            .conn
            .query_row(
                "SELECT current_turn FROM execution_lanes WHERE lane=?1",
                [&position.lane],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Absence allows a new lane; known richer context is never silently replaced.
    pub fn execution_lane_context_matches(
        &self,
        position: &ExecutionPosition,
    ) -> Result<bool, LedgerError> {
        let context: Option<String> = self
            .conn
            .query_row(
                "SELECT exact_context FROM execution_lanes WHERE lane=?1",
                [&position.lane],
                |r| r.get(0),
            )
            .optional()?;
        Ok(context
            .as_ref()
            .is_none_or(|context| context == &position.context))
    }

    pub fn execution_lane_exists(&self, position: &ExecutionPosition) -> Result<bool, LedgerError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM execution_lanes WHERE lane=?1)",
            [&position.lane],
            |r| r.get(0),
        )?)
    }

    pub fn execution_position_matches(
        &self,
        position: &ExecutionPosition,
    ) -> Result<bool, LedgerError> {
        let exact: Option<String> = self
            .conn
            .query_row(
                "SELECT exact_identity FROM execution_turns WHERE lane=?1 AND turn=?2",
                rusqlite::params![position.lane, position.turn],
                |r| r.get(0),
            )
            .optional()?;
        Ok(exact.as_deref() == Some(position.exact.as_str()))
    }
    fn require_owner_transaction(&self) -> Result<(), LedgerError> {
        if self.conn.is_autocommit() {
            Err(invalid())
        } else {
            Ok(())
        }
    }

    /// Existing economics sees an isolated internal lane, never the raw native
    /// session ID. This token is routing only; owner tables establish attribution.
    pub fn execution_lane_session(position: &ExecutionPosition) -> String {
        format!("execution-owner-v1:{}", position.lane)
    }

    pub fn execution_turn_state(
        &self,
        position: &ExecutionPosition,
    ) -> Result<Option<String>, LedgerError> {
        self.execution_turn_state_for(&position.lane, &position.turn)
    }

    /// Same lookup as [`Self::execution_turn_state`], but for an arbitrary
    /// `(lane, turn)` pair rather than an incoming `ExecutionPosition`'s own
    /// turn. HORO-1714 decision B needs this for the lane's *previous*
    /// turn (the one `supersedes_turn` would have named, had Codex sent
    /// one) -- a different turn than the new prompt's own `position.turn`.
    pub fn execution_turn_state_for(
        &self,
        lane: &str,
        turn: &str,
    ) -> Result<Option<String>, LedgerError> {
        Ok(self
            .conn
            .query_row(
                "SELECT state FROM execution_turns WHERE lane=?1 AND turn=?2",
                rusqlite::params![lane, turn],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn resolve_execution(
        &self,
        position: &ExecutionPosition,
    ) -> Result<ExecutionLookup, LedgerError> {
        let row: Option<(String, String, String, String, String)> = self
            .conn
            .query_row(
                "SELECT t.exact_identity,t.identity_json,t.task_id,t.initial_plan_id,t.state
             FROM execution_turns t JOIN execution_lanes l ON l.lane=t.lane
             WHERE t.lane=?1 AND t.turn=?2 AND l.current_turn=t.turn",
                rusqlite::params![position.lane, position.turn],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((exact, identity_json, task, initial, state)) = row else {
            return Ok(ExecutionLookup::Unavailable(
                if self.execution_turn_state(position)?.is_some() {
                    AssociationUnavailable::Stale
                } else {
                    AssociationUnavailable::Missing
                },
            ));
        };
        if exact != position.exact {
            return Ok(ExecutionLookup::Unavailable(
                AssociationUnavailable::Ambiguous,
            ));
        }
        if state != "active" {
            return Ok(ExecutionLookup::Unavailable(AssociationUnavailable::Stale));
        }
        let identity: ExecutionIdentity =
            serde_json::from_str(&identity_json).map_err(|_| invalid())?;
        if ExecutionPosition::from_identity(&identity).as_ref() != Ok(position) {
            return Ok(ExecutionLookup::Unavailable(
                AssociationUnavailable::Ambiguous,
            ));
        }
        let task_id = TaskId(uuid::Uuid::parse_str(&task).map_err(|_| invalid())?);
        let initial_plan_id = PlanId(uuid::Uuid::parse_str(&initial).map_err(|_| invalid())?);
        let session = Self::execution_lane_session(position);
        let mut statement = self.conn.prepare("SELECT plan_id,task_id FROM session_preflights WHERE session_id=?1 AND status='in_flight'")?;
        let plans: Vec<(String, String)> = statement
            .query_map([&session], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        if plans.len() != 1 || plans[0].1 != task {
            return Ok(ExecutionLookup::Unavailable(
                AssociationUnavailable::Ambiguous,
            ));
        }
        let plan_id = PlanId(uuid::Uuid::parse_str(&plans[0].0).map_err(|_| invalid())?);
        if self.task_id_for_session(&session)? != Some(task_id)
            || self.get_plan(plan_id)?.map(|p| p.task_id) != Some(task_id)
            || self.get_plan(initial_plan_id)?.map(|p| p.task_id) != Some(task_id)
        {
            return Ok(ExecutionLookup::Unavailable(
                AssociationUnavailable::Ambiguous,
            ));
        }
        Ok(ExecutionLookup::Found(ExecutionTarget {
            task_id,
            plan_id,
            initial_plan_id,
            lineage_status: identity.lineage_status(),
            parent_agent_id: identity.parent_agent_id().map(str::to_owned),
        }))
    }

    /// HORO-1714 decision B (2026-10-10): `expected_current` is a
    /// compare-and-swap guard, not merely a read the caller already did.
    /// Atomicity against a concurrent racer does not depend on the daemon
    /// having read the right value a moment earlier -- it depends on this
    /// single statement, inside the one `BEGIN IMMEDIATE` transaction this
    /// method requires, refusing to move the lane unless the row it's
    /// actually updating still matches. Pass `None` for a brand-new lane
    /// (no existing row to guard against); pass `Some(finalized_turn)` for
    /// an owner-managed succession with no native predecessor field.
    pub fn bind_execution_turn(
        &mut self,
        position: &ExecutionPosition,
        identity: &ExecutionIdentity,
        task_id: TaskId,
        plan_id: PlanId,
        expected_current: Option<&str>,
    ) -> Result<(), LedgerError> {
        self.require_owner_transaction()?;
        self.conn.execute(
            "UPDATE execution_turns SET state='superseded' WHERE lane=?1
             AND turn=(SELECT current_turn FROM execution_lanes WHERE lane=?1)
             AND state IN ('active','finalized')",
            [&position.lane],
        )?;
        let rows = self.conn.execute(
            "INSERT INTO execution_lanes(lane,current_turn,exact_context) VALUES(?1,?2,?3)
            ON CONFLICT(lane) DO UPDATE SET current_turn=excluded.current_turn
            WHERE execution_lanes.current_turn IS ?4",
            rusqlite::params![
                position.lane,
                position.turn,
                position.context,
                expected_current
            ],
        )?;
        if rows != 1 {
            return Err(LedgerError::InvalidExecutionAssociation);
        }
        self.conn.execute("INSERT INTO execution_turns(lane,turn,exact_identity,identity_json,task_id,initial_plan_id,state)
            VALUES(?1,?2,?3,?4,?5,?6,'active')",
            rusqlite::params![position.lane,position.turn,position.exact,encode(identity)?,task_id.to_string(),plan_id.0.to_string()])?;
        Ok(())
    }

    pub fn finalize_execution_turn(
        &mut self,
        position: &ExecutionPosition,
    ) -> Result<(), LedgerError> {
        self.require_owner_transaction()?;
        self.conn.execute("UPDATE execution_turns SET state='finalized' WHERE lane=?1 AND turn=?2 AND state='active'",
            rusqlite::params![position.lane,position.turn])?;
        self.supersede_in_flight_preflights(&Self::execution_lane_session(position))?;
        Ok(())
    }

    pub fn execution_replay(
        &self,
        position: &ExecutionPosition,
        operation: &str,
        native_ref: &str,
    ) -> Result<Option<(String, ExecutionTarget)>, LedgerError> {
        let row: Option<(String,String)> = self.conn.query_row(
            "SELECT turn,target_json FROM execution_replays WHERE lane=?1 AND operation=?2 AND native_ref=?3",
            rusqlite::params![position.lane,operation,native_ref], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        row.map(|(turn, json)| Ok((turn, serde_json::from_str(&json).map_err(|_| invalid())?)))
            .transpose()
    }

    pub fn record_execution_replay(
        &mut self,
        position: &ExecutionPosition,
        operation: &str,
        native_ref: &str,
        target: &ExecutionTarget,
    ) -> Result<(), LedgerError> {
        self.require_owner_transaction()?;
        self.conn.execute("INSERT INTO execution_replays(lane,operation,native_ref,turn,target_json) VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![position.lane,operation,native_ref,position.turn,encode(target)?])?;
        Ok(())
    }
}
