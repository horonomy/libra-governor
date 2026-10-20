-- Usage-denomination marker on finalized receipts, added for HORO-1727
-- (scope-separated resource policy, history-based calibration).
--
-- Before HORO-1882 (PR #153), the transcript scan summed every content
-- block line of a turn, so stored `actual_usage_json` token figures are
-- inflated (~2.15x measured on the dogfood corpus) and not comparable to
-- today's deduplicated fresh-token figures
-- (input + cache_creation + output, cache_read excluded).
--
-- `receipts.usage_basis` names the denomination of `actual_usage_json`.
-- Receipts written by a build that knows this column carry
-- `fresh_dedup_v1`. Existing rows get NULL, NOT a guessed value and NOT a
-- rescaled figure: `receipts_for_estimation` treats a NULL basis as
-- "resource usage not comparable" and withholds the usage (duration and
-- every other field are unaffected). The filter is read-side only, so it
-- is fully reversible and no stored measurement is altered.

ALTER TABLE receipts ADD COLUMN usage_basis TEXT;
