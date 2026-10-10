//! Multi-session isolation tests (HORO-1604): proves concurrent Claude
//! Code sessions sharing one on-disk ledger never cross-contaminate each
//! other's session -> task resolution or session-scoped economic
//! reports. Mirrors the pattern already landed for Circinus
//! (`circinus#238`) and Fornax (`fornax-core PR #241`), applied to
//! `LedgerStore::resolve_or_create_task_for_session`,
//! `task_id_for_session`, and `economic_truth_for_session`.
//!
//! Each thread opens its own [`LedgerStore`] connection against the same
//! on-disk SQLite file, exactly as `reservation_concurrency.rs` does --
//! `Connection` is not `Sync`, so a real cross-session race must be
//! exercised as separate connections, not separate in-process calls on
//! one shared store.

use std::path::PathBuf;
use std::sync::{Arc, Barrier};

use libra_governor_ledger::LedgerStore;
use time::OffsetDateTime;

fn now() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH
}

/// Eight concurrent sessions, each racing to resolve-or-create its own
/// task against the same ledger file, then re-reading `task_id_for_session`
/// and `economic_truth_for_session` under continued concurrent writes from
/// every other session. Every thread must see only its own task for the
/// entire run -- never another session's, never a stale/none result after
/// its own first successful resolve.
#[test]
fn concurrent_distinct_sessions_never_resolve_each_others_task() {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("ledger.sqlite");
    // Create the file and run migrations once up front so every thread's
    // first `LedgerStore::open` races on an already-initialized schema,
    // not on `CREATE TABLE` contention.
    drop(LedgerStore::open(&path).unwrap());

    const SESSIONS: usize = 8;
    const ROUNDS: usize = 25;
    let barrier = Arc::new(Barrier::new(SESSIONS));

    let handles: Vec<_> = (0..SESSIONS)
        .map(|i| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let session_id = format!("session-{i}");
                let mut store = LedgerStore::open(&path).unwrap();

                barrier.wait();

                let own_task = store
                    .resolve_or_create_task_for_session(&session_id, now())
                    .unwrap();

                for _ in 0..ROUNDS {
                    let resolved = store
                        .resolve_or_create_task_for_session(&session_id, now())
                        .unwrap();
                    assert_eq!(
                        resolved, own_task,
                        "session {session_id} resolved a different task on a later call \
                         (re-resolve must be idempotent even under concurrent writes from \
                         other sessions)"
                    );

                    let looked_up = store.task_id_for_session(&session_id).unwrap();
                    assert_eq!(
                        looked_up,
                        Some(own_task),
                        "session {session_id} saw task {looked_up:?} instead of its own \
                         {own_task:?} -- cross-session leakage"
                    );

                    let truth = store.economic_truth_for_session(&session_id).unwrap();
                    assert_eq!(
                        truth.selector.selector, "session",
                        "economic_truth_for_session must echo the session selector, not task"
                    );
                    assert_eq!(
                        truth.selector.value, session_id,
                        "economic_truth_for_session echoed a different session id -- \
                         cross-session leakage in the report identity itself"
                    );
                }

                own_task
            })
        })
        .collect();

    let resolved_tasks: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for i in 0..SESSIONS {
        for j in (i + 1)..SESSIONS {
            assert_ne!(
                resolved_tasks[i], resolved_tasks[j],
                "two distinct sessions resolved to the same task id"
            );
        }
    }
}

/// Anti-vacuity guard (HORO-1604 failure mode: "query selecting
/// globally-latest row"). Creates session `"old"` first, then session
/// `"new"` second (strictly later `created_at`, and inserted later into
/// `session_tasks`). A buggy `task_id_for_session` that resolved by
/// "most recently created session_tasks row" instead of filtering on
/// `session_id` would return `new`'s task when asked for `old`'s. This
/// test fails under that exact mutation and only passes under the real,
/// session-scoped `WHERE session_id = ?1` query -- it is not a vacuous
/// assertion that any implementation would satisfy.
#[test]
fn session_lookup_is_never_satisfied_by_the_globally_latest_row() {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("ledger.sqlite");
    let mut store = LedgerStore::open(&path).unwrap();

    let t0 = now();
    let t1 = t0 + time::Duration::seconds(60);

    let old_task = store.resolve_or_create_task_for_session("old", t0).unwrap();
    let new_task = store.resolve_or_create_task_for_session("new", t1).unwrap();
    assert_ne!(old_task, new_task);

    // "new" is strictly the most-recently-created session_tasks row.
    // Asking for "old" must still return "old"'s task, not "new"'s.
    let looked_up_old = store.task_id_for_session("old").unwrap();
    assert_eq!(
        looked_up_old,
        Some(old_task),
        "task_id_for_session(\"old\") returned the globally-latest row's task instead of \
         old's own task"
    );

    let truth_old = store.economic_truth_for_session("old").unwrap();
    assert_eq!(truth_old.selector.value, "old");
}

/// A session that never called `resolve_or_create_task_for_session` must
/// report `NoSuchSession`, never silently borrow another session's
/// account or task just because one exists in the same ledger file.
#[test]
fn unknown_session_never_borrows_another_sessions_task() {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("ledger.sqlite");
    let mut store = LedgerStore::open(&path).unwrap();

    store
        .resolve_or_create_task_for_session("known", now())
        .unwrap();

    assert_eq!(store.task_id_for_session("unknown").unwrap(), None);

    let truth = store.economic_truth_for_session("unknown").unwrap();
    assert_eq!(truth.selector.selector, "session");
    assert_eq!(truth.selector.value, "unknown");
    assert_eq!(
        truth.resolution,
        libra_governor_domain::ScopeResolution::NoBasis {
            reason: libra_governor_domain::NoEconomicBasis::NoSuchSession
        },
        "unknown session must report NoSuchSession, not fall through to any real account"
    );
}
