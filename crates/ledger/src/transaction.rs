use std::ops::Deref;

use rusqlite::{Connection, Savepoint, Transaction, TransactionBehavior};

/// A transaction boundary that behaves the same for standalone operations
/// and operations composed inside a larger ledger transaction.
pub(crate) enum TransactionAdapter<'conn> {
    Transaction(Transaction<'conn>),
    Savepoint(Savepoint<'conn>),
}

impl TransactionAdapter<'_> {
    pub(crate) fn commit(self) -> rusqlite::Result<()> {
        match self {
            Self::Transaction(transaction) => transaction.commit(),
            Self::Savepoint(savepoint) => savepoint.commit(),
        }
    }
}

impl Deref for TransactionAdapter<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Transaction(transaction) => transaction,
            Self::Savepoint(savepoint) => savepoint,
        }
    }
}

/// Starts the requested transaction when this is a top-level operation, or
/// a savepoint when composing inside an already-open ledger transaction.
pub(crate) fn begin(
    conn: &mut Connection,
    behavior: TransactionBehavior,
) -> rusqlite::Result<TransactionAdapter<'_>> {
    if conn.is_autocommit() {
        Ok(TransactionAdapter::Transaction(
            conn.transaction_with_behavior(behavior)?,
        ))
    } else {
        Ok(TransactionAdapter::Savepoint(conn.savepoint()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reservation::test_support::{fixed_reserve, thousand_token_policy},
        LedgerError, LedgerStore, ReserveOutcome, ReserveRequest,
    };
    use libra_governor_domain::{
        CompletionContract, CompletionCriterion, ReservationClass, ResourceAmount, TaskId,
        TaskIdentity,
    };
    use std::panic::AssertUnwindSafe;
    use time::OffsetDateTime;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    fn create_task(store: &mut LedgerStore) -> TaskId {
        let task_id = TaskId::new();
        store
            .insert_task(
                &TaskIdentity {
                    id: task_id,
                    external_ref: None,
                },
                now(),
            )
            .unwrap();
        task_id
    }

    #[test]
    fn standalone_transactions_keep_requested_behavior() {
        let mut conn = Connection::open_in_memory().unwrap();
        let tx = begin(&mut conn, TransactionBehavior::Immediate).unwrap();
        tx.execute_batch("CREATE TABLE example (value INTEGER)")
            .unwrap();
        tx.commit().unwrap();
        assert!(conn.is_autocommit());
    }

    #[test]
    fn owner_transaction_rolls_back_contract_budget_reservation_and_settlement() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = create_task(&mut store);
        let contract = CompletionContract::first(vec![CompletionCriterion::required("done")]);

        let result = store.with_immediate_transaction(|store| {
            store.insert_contract(task_id, &contract, now())?;
            store.initialize_task_budget(
                task_id,
                &thousand_token_policy(),
                &fixed_reserve(200),
                now(),
            )?;
            let ReserveOutcome::Granted(reservation) = store.reserve(ReserveRequest {
                task_id,
                session_id: "transaction-test",
                plan_id: None,
                class: ReservationClass::OptionalWork,
                amount: ResourceAmount::Tokens(300),
                idempotency_key: "outer-rollback",
                now: now(),
                ttl_secs: 900,
            })?
            else {
                panic!("expected reservation to be granted");
            };
            store.settle(reservation.id, Some(ResourceAmount::Tokens(250)), now())?;

            let nested_snapshot = store.budget_snapshot(task_id)?.unwrap();
            assert_eq!(nested_snapshot.used(), ResourceAmount::Tokens(250));

            Err::<(), LedgerError>(LedgerError::NegativeSettlement)
        });

        assert!(matches!(result, Err(LedgerError::NegativeSettlement)));
        let contract_count: u64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM contracts WHERE task_id = ?1",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(contract_count, 0);
        assert_eq!(store.task_budget(task_id).unwrap(), None);
        assert!(store.reservations_for_task(task_id).unwrap().is_empty());
        assert_eq!(store.budget_snapshot(task_id).unwrap(), None);
    }

    #[test]
    fn owner_transaction_rolls_back_when_callback_panics() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = create_task(&mut store);
        let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _: Result<(), LedgerError> = store.with_immediate_transaction(|store| {
                store.insert_contract(
                    task_id,
                    &CompletionContract::first(vec![CompletionCriterion::required("done")]),
                    now(),
                )?;
                panic!("simulate owner panic");
            });
        }));
        assert!(panic.is_err());

        let contract_count: u64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM contracts WHERE task_id = ?1",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(contract_count, 0);
    }

    #[test]
    fn owner_transaction_rejects_a_nested_owner_transaction() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .with_immediate_transaction(|store| {
                let nested = store.with_immediate_transaction(|_| Ok::<_, LedgerError>(()));
                assert!(matches!(nested, Err(LedgerError::Sqlite(_))));
                Ok::<_, LedgerError>(())
            })
            .unwrap();
    }
}
