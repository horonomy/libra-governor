-- Capture-time stamps for the DogFood evidence adapter (HORO-1376, per
-- ADR-0012 and the `dogfood-evidence-store-test-contract.md` /
-- `dogfood-evidence-canonicalization-v1.md` governance docs).
--
-- These columns exist so `crates/evidence-adapter` can *read* a stable
-- per-record identity, an immutable origin profile, and a genuine
-- ingestion timestamp back out of `plans`/`receipts` without ever
-- re-deriving them at read time -- ADR-0012 §3 requires `event_id` to be
-- "generated once at capture, stable across every retry" and
-- `ingested_at` to be "never equal-by-construction to `occurred_at`".
-- This migration only adds columns; it adds no new table, no upload
-- path, and no network dependency -- it is purely local capture-side
-- bookkeeping for a read-only projection layer that lives entirely
-- outside `crates/cli/src/evidence_report_cmd.rs` (ADR-0012 §11.4).
--
-- `dogfood_event_id` is nullable because a pre-HORO-1376 row was written
-- before this migration existed and genuinely has no such id -- it is
-- never backfilled with a fabricated value; the adapter simply skips
-- rows where it is NULL rather than manufacture an identity after the
-- fact (a fabricated `event_id` would violate ADR-0012 §3's "generated
-- once at capture" rule for that record).
--
-- `dogfood_origin_profile` mirrors ADR-0012 §3/§8: captured once, at
-- insert time, from `LIBRA_GOVERNOR_DOGFOOD_PROFILE` (default
-- `personal`, exact match `corporate`) -- never re-derived from the
-- live environment when the adapter later reads the row, which is
-- exactly what makes a corporate-origin record's ineligibility survive
-- a later profile change.
--
-- `dogfood_ingested_at` is written by a separate `UPDATE`, after the
-- row's own durable INSERT has already returned, specifically so it is
-- never equal-by-construction to `created_at`/`recorded_at` (see
-- `LedgerStore::insert_plan`/`insert_receipt` in `write.rs`).

ALTER TABLE plans ADD COLUMN dogfood_event_id TEXT;
ALTER TABLE plans ADD COLUMN dogfood_origin_profile TEXT;
ALTER TABLE plans ADD COLUMN dogfood_ingested_at TEXT;

ALTER TABLE receipts ADD COLUMN dogfood_event_id TEXT;
ALTER TABLE receipts ADD COLUMN dogfood_origin_profile TEXT;
ALTER TABLE receipts ADD COLUMN dogfood_ingested_at TEXT;
