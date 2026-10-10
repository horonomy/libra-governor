//! Adversarial probe for `outcome_attestations`' append-only trigger
//! (ADR-0017, HORO-1727, migration 0018), mirroring
//! `renewal_adversarial.rs`'s established raw-SQL-probe pattern.
//!
//! Lives in `tests/`, not `crates/ledger/src/extension.rs`'s own unit
//! tests, because a raw `DELETE FROM outcome_attestations` string
//! literal in `src/` would trip
//! `crates/evidence-adapter/tests/adapter_fixtures.rs`'s
//! `dropped_count_zero_is_provable_no_eviction_path_exists_in_ledger`
//! guard — the same reason `renewal_adversarial.rs`'s analogous probe for
//! `task_budget_renewals` lives here instead of in `src/renewal.rs`
//! (fixed in HORO-1727 PR #108).

use libra_governor_domain::TaskIdentity;
use libra_governor_ledger::{LedgerStore, OutcomeAttestationInsert};
use time::OffsetDateTime;

fn open_file_store() -> (tempfile::TempDir, std::path::PathBuf, LedgerStore) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let store = LedgerStore::open(&path).unwrap();
    (dir, path, store)
}

fn insert_one_attestation(store: &mut LedgerStore, task_id: libra_governor_domain::TaskId) {
    store
        .insert_outcome_attestation(OutcomeAttestationInsert {
            id: "attest-1",
            task_id,
            plan_id: None,
            contract_revision: None,
            source: "provider",
            source_id: Some("example-provider"),
            outcome_kind: "completed",
            evidence_json: "[]",
            idempotency_key: "ci-run-1",
            authoritative: true,
            attested_at: OffsetDateTime::UNIX_EPOCH,
        })
        .unwrap();
}

#[test]
fn raw_update_against_outcome_attestations_is_rejected_by_the_append_only_trigger() {
    let (_dir, path, mut store) = open_file_store();
    let identity = TaskIdentity::new(None);
    let task_id = identity.id;
    store
        .insert_task(&identity, OffsetDateTime::UNIX_EPOCH)
        .unwrap();
    insert_one_attestation(&mut store, task_id);
    drop(store);

    let conn = rusqlite::Connection::open(&path).unwrap();
    let result = conn.execute(
        "UPDATE outcome_attestations SET outcome_kind = 'failed' WHERE id = 'attest-1'",
        [],
    );
    assert!(
        result.is_err(),
        "an UPDATE against outcome_attestations must be rejected by the append-only trigger"
    );
}

#[test]
fn raw_delete_against_outcome_attestations_is_rejected_by_the_append_only_trigger() {
    let (_dir, path, mut store) = open_file_store();
    let identity = TaskIdentity::new(None);
    let task_id = identity.id;
    store
        .insert_task(&identity, OffsetDateTime::UNIX_EPOCH)
        .unwrap();
    insert_one_attestation(&mut store, task_id);
    drop(store);

    let conn = rusqlite::Connection::open(&path).unwrap();
    let result = conn.execute("DELETE FROM outcome_attestations WHERE id = 'attest-1'", []);
    assert!(
        result.is_err(),
        "a DELETE against outcome_attestations must be rejected by the append-only trigger"
    );
}
