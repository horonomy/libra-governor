-- Transcript high-water mark of a settled shared-pool turn hold
-- (HORO-2196 follow-up). When a turn hold is settled from a priced
-- transcript window, this records the newest transcript RECORD timestamp
-- that window counted (RFC 3339, same text form as the other timestamp
-- columns), so the next window starts after the last record actually
-- measured rather than after the wall-clock instant of the Stop. The host
-- writes its transcript asynchronously, so records stamped before a Stop
-- are routinely flushed after it; a wall-clock watermark would lose them.
--
-- Nullable with no backfill: every row written before this migration, and
-- every hold settled without a counted window (unmeasured, unpriced,
-- charges), stays NULL and readers fall back to the earlier derivation
-- (settled_at / created_at). Deliberately separate from `settled_at`,
-- whose meaning (when the row settled) is unchanged.
ALTER TABLE shared_pool_reservations ADD COLUMN watermark_ts TEXT;

-- JSON array of the `message.id`s counted up to and including that mark
-- (bounded by the writer). One API message is written as several transcript
-- lines (one per content block) that can straddle the mark when the host
-- flushes asynchronously; a later window skips tail lines of an id already
-- counted instead of counting the message twice. NULL with no backfill.
ALTER TABLE shared_pool_reservations ADD COLUMN watermark_message_ids TEXT;
