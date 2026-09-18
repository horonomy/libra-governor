//! Session -> task resolution and in-flight preflight bookkeeping.
//!
//! Backs the Claude Code hook integration (HORO-1125): a host agent
//! `session_id` is opaque and resets across restarts, but one task may
//! span many prompts within a session (see `docs/adr/0002`). These
//! methods let a second `Preflight` request for the same `session_id`
//! resolve to the same [`TaskId`] and supersede the prior in-flight
//! preflight rather than leaving it dangling — see
//! `migrations/0002_session_preflight_state.sql`.

use libra_governor_domain::{
    CompletionContract, PlanId, ReplanHysteresisState, TaskId, TaskIdentity,
};
use time::format_description::well_known::Rfc3339;

use crate::{store::LedgerStore, LedgerError};

fn rfc3339(t: time::OffsetDateTime) -> Result<String, LedgerError> {
    t.format(&Rfc3339)
        .map_err(|e| LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))
}

impl LedgerStore {
    /// Returns the [`TaskId`] already associated with `session_id`, or
    /// creates a brand new [`TaskIdentity`] (and the mapping) if this is
    /// the session's first preflight. Never inserts a duplicate `tasks`
    /// row for a session seen before — `insert_task` is a plain `INSERT`
    /// and errors on a repeat, so this must check first.
    pub fn resolve_or_create_task_for_session(
        &mut self,
        session_id: &str,
        now: time::OffsetDateTime,
    ) -> Result<TaskId, LedgerError> {
        if let Some(existing) = self.task_id_for_session(session_id)? {
            return Ok(existing);
        }

        let identity = TaskIdentity::new(None);
        self.insert_task(&identity, now)?;
        self.conn.execute(
            "INSERT INTO session_tasks (session_id, task_id, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![session_id, identity.id.to_string(), rfc3339(now)?],
        )?;
        Ok(identity.id)
    }

    /// Looks up the [`TaskId`] already bound to `session_id`, if any.
    pub fn task_id_for_session(&self, session_id: &str) -> Result<Option<TaskId>, LedgerError> {
        let task_id: Option<String> = self
            .conn
            .query_row(
                "SELECT task_id FROM session_tasks WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .ok();
        task_id
            .map(|s| {
                uuid::Uuid::parse_str(&s)
                    .map(TaskId)
                    .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
            })
            .transpose()
    }

    /// Returns the highest-revision [`CompletionContract`] recorded for
    /// `task_id`, if any contract has been recorded yet.
    pub fn latest_contract(
        &self,
        task_id: TaskId,
    ) -> Result<Option<CompletionContract>, LedgerError> {
        let task_id_str = task_id.to_string();
        let revision: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(revision) FROM contracts WHERE task_id = ?1",
                [&task_id_str],
                |row| row.get(0),
            )
            .ok()
            .flatten();

        let Some(revision) = revision else {
            return Ok(None);
        };

        let mut stmt = self.conn.prepare(
            "SELECT description, required FROM contract_criteria
             WHERE task_id = ?1 AND revision = ?2 ORDER BY ordinal ASC",
        )?;
        let criteria = stmt
            .query_map(rusqlite::params![task_id_str, revision], |row| {
                Ok(libra_governor_domain::CompletionCriterion {
                    description: row.get(0)?,
                    required: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Some(CompletionContract {
            revision: revision as u32,
            criteria,
        }))
    }

    /// Marks every currently `in_flight` preflight for `session_id` as
    /// `superseded`. Call this before recording a new preflight for the
    /// same session so at most one preflight is ever `in_flight` per
    /// session — the mechanism that prevents orphaned "active estimate"
    /// state when a user submits a new prompt or cancels mid-session.
    pub fn supersede_in_flight_preflights(&mut self, session_id: &str) -> Result<(), LedgerError> {
        self.conn.execute(
            "UPDATE session_preflights SET status = 'superseded'
             WHERE session_id = ?1 AND status = 'in_flight'",
            [session_id],
        )?;
        Ok(())
    }

    /// Records `plan_id` as the new `in_flight` preflight for
    /// `session_id`. Callers must call [`Self::supersede_in_flight_preflights`]
    /// first so this never leaves two preflights `in_flight` for one
    /// session at once.
    pub fn record_preflight(
        &mut self,
        session_id: &str,
        task_id: TaskId,
        plan_id: PlanId,
        now: time::OffsetDateTime,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO session_preflights (session_id, plan_id, task_id, status, created_at)
             VALUES (?1, ?2, ?3, 'in_flight', ?4)",
            rusqlite::params![
                session_id,
                plan_id.0.to_string(),
                task_id.to_string(),
                rfc3339(now)?,
            ],
        )?;
        Ok(())
    }

    /// Returns when `session_id` first resolved a task (i.e. its first
    /// `Preflight`), if it has ever done so. Used as "task start" for
    /// elapsed-duration computation at `Finalize` — see
    /// [`Self::resolve_or_create_task_for_session`], which is the only
    /// writer of `session_tasks.created_at` and writes it exactly once
    /// per session.
    pub fn session_started_at(
        &self,
        session_id: &str,
    ) -> Result<Option<time::OffsetDateTime>, LedgerError> {
        let created_at: Option<String> = self
            .conn
            .query_row(
                "SELECT created_at FROM session_tasks WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .ok();
        created_at
            .map(|s| {
                time::OffsetDateTime::parse(&s, &Rfc3339)
                    .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
            })
            .transpose()
    }

    /// Increments the fire-and-forget per-session tool-call counter.
    /// Deliberately a single cheap upsert with no session -> task lookup,
    /// so this stays fast enough not to add perceptible latency to every
    /// tool call (`hook post-tool-use`, HORO-1126).
    pub fn increment_tool_call_count(&mut self, session_id: &str) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO tool_call_counts (session_id, count) VALUES (?1, 1)
             ON CONFLICT(session_id) DO UPDATE SET count = count + 1",
            [session_id],
        )?;
        Ok(())
    }

    /// Returns the current tool-call count for `session_id` (`0` if none
    /// has been recorded).
    pub fn tool_call_count_for_session(&self, session_id: &str) -> Result<u64, LedgerError> {
        let count: Option<i64> = self
            .conn
            .query_row(
                "SELECT count FROM tool_call_counts WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .ok();
        Ok(count.unwrap_or(0) as u64)
    }

    /// Records one tool invocation for `session_id`'s same-tool streak
    /// (HORO-1139): if `tool_name` matches the last-invoked tool, the
    /// streak increments; otherwise it resets to `1`. Returns the
    /// resulting streak count. The streak is deliberately not
    /// time-windowed beyond "no different tool invoked in between" — see
    /// `libra_governor_domain::replan` module docs for why a consecutive
    /// streak, not a wall-clock window, is the signal used here (Claude
    /// Code's `PostToolUse` payload carries no reliable elapsed-time
    /// field cheap enough to reason about per call).
    ///
    /// `last_tool_at` is persisted alongside purely as diagnostic
    /// metadata (when this session's streak-relevant tool last ran) — it
    /// is not currently read back by any windowing logic, honestly
    /// documented here rather than implying a time window exists when it
    /// does not.
    pub fn record_tool_invocation(
        &mut self,
        session_id: &str,
        tool_name: &str,
        now: time::OffsetDateTime,
    ) -> Result<u64, LedgerError> {
        let previous: Option<String> = self
            .conn
            .query_row(
                "SELECT last_tool_name FROM tool_call_counts WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .ok()
            .flatten();

        let streak: i64 = if previous.as_deref() == Some(tool_name) {
            self.conn.query_row(
                "SELECT same_tool_streak FROM tool_call_counts WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )?
        } else {
            0
        } + 1;

        self.conn.execute(
            "INSERT INTO tool_call_counts (session_id, count, last_tool_name, last_tool_at, same_tool_streak)
             VALUES (?1, 0, ?2, ?3, ?4)
             ON CONFLICT(session_id) DO UPDATE SET
                last_tool_name = excluded.last_tool_name,
                last_tool_at = excluded.last_tool_at,
                same_tool_streak = excluded.same_tool_streak",
            rusqlite::params![session_id, tool_name, rfc3339(now)?, streak],
        )?;
        Ok(streak as u64)
    }

    /// Returns the current per-task [`ReplanHysteresisState`] (HORO-1139):
    /// how many automatic replans this task has already had and when the
    /// most recent one happened. `ReplanHysteresisState::default()`
    /// (never replanned) if no `replan_state` row exists yet.
    pub fn replan_state_for_task(
        &self,
        task_id: TaskId,
    ) -> Result<ReplanHysteresisState, LedgerError> {
        let row: Option<(i64, Option<String>, i64)> = self
            .conn
            .query_row(
                "SELECT auto_replan_count, last_replan_at, tool_call_count_at_last_replan
                 FROM replan_state WHERE task_id = ?1",
                [task_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();

        let Some((auto_replan_count, last_replan_at, tool_call_count_at_last_replan)) = row else {
            return Ok(ReplanHysteresisState::default());
        };
        let last_replan_at = last_replan_at
            .map(|s| {
                time::OffsetDateTime::parse(&s, &Rfc3339)
                    .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
            })
            .transpose()?;

        Ok(ReplanHysteresisState {
            auto_replan_count: auto_replan_count as u32,
            last_replan_at,
            tool_call_count_at_last_replan: tool_call_count_at_last_replan as u64,
        })
    }

    /// Resets `session_id`'s same-tool streak to `0` (HORO-1139): called
    /// right after a replan triggered by [`possible_tool_loop`], so the
    /// loop signal re-baselines the same way the tool-call-count signal
    /// does via `record_replan_for_task`'s
    /// `tool_call_count_at_last_replan` — otherwise the very next
    /// repeated tool call would immediately look like a continuation of
    /// the already-replanned-on streak instead of fresh evidence.
    ///
    /// [`possible_tool_loop`]: libra_governor_domain::possible_tool_loop
    pub fn reset_tool_streak(&mut self, session_id: &str) -> Result<(), LedgerError> {
        self.conn.execute(
            "UPDATE tool_call_counts SET same_tool_streak = 0 WHERE session_id = ?1",
            [session_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::CompletionCriterion;

    fn now() -> time::OffsetDateTime {
        time::OffsetDateTime::UNIX_EPOCH
    }

    #[test]
    fn resolve_or_create_returns_same_task_id_for_repeat_session() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let first = store
            .resolve_or_create_task_for_session("sess-1", now())
            .unwrap();
        let second = store
            .resolve_or_create_task_for_session("sess-1", now())
            .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn resolve_or_create_gives_distinct_ids_for_distinct_sessions() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let a = store
            .resolve_or_create_task_for_session("sess-a", now())
            .unwrap();
        let b = store
            .resolve_or_create_task_for_session("sess-b", now())
            .unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn latest_contract_is_none_before_any_contract_recorded() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = store
            .resolve_or_create_task_for_session("sess-1", now())
            .unwrap();
        assert_eq!(store.latest_contract(task_id).unwrap(), None);
    }

    #[test]
    fn latest_contract_returns_highest_revision() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = store
            .resolve_or_create_task_for_session("sess-1", now())
            .unwrap();
        let v1 = CompletionContract::first(vec![CompletionCriterion::required("v1")]);
        store.insert_contract(task_id, &v1, now()).unwrap();
        let v2 = v1.next_revision(vec![CompletionCriterion::required("v2")]);
        store.insert_contract(task_id, &v2, now()).unwrap();

        let latest = store.latest_contract(task_id).unwrap().unwrap();
        assert_eq!(latest.revision, 2);
        assert_eq!(latest.criteria[0].description, "v2");
    }

    #[test]
    fn supersede_marks_prior_in_flight_preflight_before_new_one_recorded() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = store
            .resolve_or_create_task_for_session("sess-1", now())
            .unwrap();
        let contract = CompletionContract::first(vec![CompletionCriterion::required("c")]);
        store.insert_contract(task_id, &contract, now()).unwrap();

        let plan1 = libra_governor_domain::ExecutionPlan::new(task_id, 1, None, now());
        store.insert_plan(&plan1).unwrap();
        store
            .record_preflight("sess-1", task_id, plan1.id, now())
            .unwrap();

        store.supersede_in_flight_preflights("sess-1").unwrap();

        let plan2 = libra_governor_domain::ExecutionPlan::new(task_id, 1, None, now());
        store.insert_plan(&plan2).unwrap();
        store
            .record_preflight("sess-1", task_id, plan2.id, now())
            .unwrap();

        let in_flight_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM session_preflights WHERE session_id = ?1 AND status = 'in_flight'",
                ["sess-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            in_flight_count, 1,
            "at most one preflight may be in_flight per session"
        );

        let superseded_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM session_preflights WHERE session_id = ?1 AND status = 'superseded'",
                ["sess-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(superseded_count, 1);
    }

    #[test]
    fn session_started_at_is_none_before_any_preflight() {
        let store = LedgerStore::open_in_memory().unwrap();
        assert_eq!(store.session_started_at("sess-1").unwrap(), None);
    }

    #[test]
    fn session_started_at_returns_first_resolution_time() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .resolve_or_create_task_for_session("sess-1", now())
            .unwrap();
        assert_eq!(store.session_started_at("sess-1").unwrap(), Some(now()));
    }

    #[test]
    fn tool_call_count_starts_at_zero_and_increments() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        assert_eq!(store.tool_call_count_for_session("sess-1").unwrap(), 0);
        store.increment_tool_call_count("sess-1").unwrap();
        store.increment_tool_call_count("sess-1").unwrap();
        store.increment_tool_call_count("sess-1").unwrap();
        assert_eq!(store.tool_call_count_for_session("sess-1").unwrap(), 3);
    }

    #[test]
    fn tool_call_counts_are_independent_per_session() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store.increment_tool_call_count("sess-a").unwrap();
        store.increment_tool_call_count("sess-b").unwrap();
        store.increment_tool_call_count("sess-b").unwrap();
        assert_eq!(store.tool_call_count_for_session("sess-a").unwrap(), 1);
        assert_eq!(store.tool_call_count_for_session("sess-b").unwrap(), 2);
    }

    #[test]
    fn record_tool_invocation_streak_increments_on_repeats_and_resets_on_a_new_tool() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        assert_eq!(
            store
                .record_tool_invocation("sess-1", "Bash", now())
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .record_tool_invocation("sess-1", "Bash", now())
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .record_tool_invocation("sess-1", "Bash", now())
                .unwrap(),
            3
        );
        assert_eq!(
            store
                .record_tool_invocation("sess-1", "Read", now())
                .unwrap(),
            1,
            "a different tool must reset the streak"
        );
        assert_eq!(
            store
                .record_tool_invocation("sess-1", "Read", now())
                .unwrap(),
            2
        );
    }

    #[test]
    fn record_tool_invocation_streaks_are_independent_per_session() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .record_tool_invocation("sess-a", "Bash", now())
            .unwrap();
        store
            .record_tool_invocation("sess-a", "Bash", now())
            .unwrap();
        assert_eq!(
            store
                .record_tool_invocation("sess-b", "Bash", now())
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .record_tool_invocation("sess-a", "Bash", now())
                .unwrap(),
            3
        );
    }

    #[test]
    fn replan_state_for_task_defaults_when_never_replanned() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = store
            .resolve_or_create_task_for_session("sess-1", now())
            .unwrap();
        assert_eq!(
            store.replan_state_for_task(task_id).unwrap(),
            libra_governor_domain::ReplanHysteresisState::default()
        );
    }

    #[test]
    fn record_replan_for_task_increments_count_and_sets_last_replan_at() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = store
            .resolve_or_create_task_for_session("sess-1", now())
            .unwrap();

        store.record_replan_for_task(task_id, now(), 7).unwrap();
        let state = store.replan_state_for_task(task_id).unwrap();
        assert_eq!(state.auto_replan_count, 1);
        assert_eq!(state.last_replan_at, Some(now()));
        assert_eq!(state.tool_call_count_at_last_replan, 7);

        let later = now() + time::Duration::seconds(60);
        store.record_replan_for_task(task_id, later, 15).unwrap();
        let state = store.replan_state_for_task(task_id).unwrap();
        assert_eq!(state.auto_replan_count, 2);
        assert_eq!(state.last_replan_at, Some(later));
        assert_eq!(state.tool_call_count_at_last_replan, 15);
    }

    #[test]
    fn reset_tool_streak_zeroes_the_streak_but_not_the_count() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .record_tool_invocation("sess-1", "Bash", now())
            .unwrap();
        store
            .record_tool_invocation("sess-1", "Bash", now())
            .unwrap();
        store.increment_tool_call_count("sess-1").unwrap();

        store.reset_tool_streak("sess-1").unwrap();

        assert_eq!(
            store
                .record_tool_invocation("sess-1", "Bash", now())
                .unwrap(),
            1,
            "streak restarts fresh after a reset even though the same tool repeats"
        );
        assert_eq!(
            store.tool_call_count_for_session("sess-1").unwrap(),
            1,
            "the plain cumulative count is untouched by a streak reset"
        );
    }
}
