//! Ready-task ordering for the pacing simulator (HORO-1765).
//!
//! A task is "ready" once every task in its `depends_on` set has already
//! completed (see [`PacingEvent::TaskCompleted`](super::PacingEvent) in
//! the state machine added by this ticket's second PR). This module only
//! answers *in what order* ready tasks are offered to the forecast/step
//! logic — never whether to admit one, which stays `forecast`'s job.

use std::collections::HashSet;

use super::{SimTask, SimTaskId, TaskSet};
use crate::business_context::Priority;

/// [`Priority`] does not derive `Ord` (it is recorded metadata almost
/// everywhere else in this crate — see that type's own docs) so this
/// module defines its own total order locally, scoped to ready-ordering
/// only, rather than changing a type shared by every other caller.
fn priority_rank(p: Priority) -> u8 {
    match p {
        Priority::Low => 0,
        Priority::Normal => 1,
        Priority::High => 2,
        Priority::Urgent => 3,
    }
}

/// Returns the ready subset of `tasks` (every `depends_on` entry present
/// in `completed`), ordered `(priority desc, deadline asc, id asc)` —
/// the same tie-break order named in HORO-1765's acceptance criteria.
/// `None` deadlines sort after any `Some` deadline (an undated task never
/// jumps ahead of one with a known deadline).
pub fn ready_order<'a>(tasks: &'a TaskSet, completed: &HashSet<SimTaskId>) -> Vec<&'a SimTask> {
    let mut ready: Vec<&SimTask> = tasks
        .tasks()
        .iter()
        .filter(|t| {
            !completed.contains(&t.id) && t.depends_on.iter().all(|d| completed.contains(d))
        })
        .collect();
    ready.sort_by(|a, b| {
        priority_rank(b.priority)
            .cmp(&priority_rank(a.priority))
            .then_with(|| match (a.deadline, b.deadline) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.id.cmp(&b.id))
    });
    ready
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economic_attribution::PrincipalId;
    use crate::pacing::tests::test_estimate;

    fn task(id: u32, priority: Priority, deadline: Option<time::OffsetDateTime>) -> SimTask {
        SimTask {
            id: SimTaskId(id),
            principal: PrincipalId("p".to_string()),
            depends_on: vec![],
            priority,
            deadline,
            estimate: test_estimate(),
        }
    }

    #[test]
    fn orders_by_priority_then_deadline_then_id() {
        let early = time::OffsetDateTime::UNIX_EPOCH;
        let late = early + time::Duration::seconds(100);
        let set = TaskSet::validated(vec![
            task(3, Priority::Normal, Some(late)),
            task(2, Priority::High, Some(early)),
            task(1, Priority::High, Some(late)),
            task(4, Priority::Normal, None),
        ])
        .unwrap();
        let ready = ready_order(&set, &HashSet::new());
        let ids: Vec<u32> = ready.iter().map(|t| t.id.0).collect();
        assert_eq!(ids, vec![2, 1, 3, 4]);
    }

    #[test]
    fn excludes_tasks_with_unmet_dependencies_and_already_completed_tasks() {
        let set = TaskSet::validated(vec![
            SimTask {
                id: SimTaskId(1),
                principal: PrincipalId("p".to_string()),
                depends_on: vec![],
                priority: Priority::Normal,
                deadline: None,
                estimate: test_estimate(),
            },
            SimTask {
                id: SimTaskId(2),
                principal: PrincipalId("p".to_string()),
                depends_on: vec![SimTaskId(1)],
                priority: Priority::Normal,
                deadline: None,
                estimate: test_estimate(),
            },
        ])
        .unwrap();
        let none_done = ready_order(&set, &HashSet::new());
        assert_eq!(
            none_done.iter().map(|t| t.id.0).collect::<Vec<_>>(),
            vec![1]
        );

        let mut completed = HashSet::new();
        completed.insert(SimTaskId(1));
        let after_first = ready_order(&set, &completed);
        assert_eq!(
            after_first.iter().map(|t| t.id.0).collect::<Vec<_>>(),
            vec![2]
        );
    }
}
