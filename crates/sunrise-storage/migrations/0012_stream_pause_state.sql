-- Sunrise local storage schema, migration to version 12: Stream pause state
-- and review cadence.
-- Per the migrations doc: schema changes are always a NEW migration file,
-- never an edit of an existing one.
--
-- `Stream` has carried `paused`, `paused_until` and `review_cadence` on the
-- entity (and therefore inside every stream.create / stream.update op) since
-- v1, but the materialized `streams` projection never had columns for them.
-- The row reader consequently hardcoded `paused: false` and
-- `review_cadence: Weekly`, so pausing a Stream survived exactly as long as the
-- process that did it: the op said "paused", the projection said "not paused",
-- and every read believed the projection.
--
-- `docs/08-features/reviews-and-stats.md` §Weekly review is the first consumer
-- that cares: it reviews "each non-archived, **non-paused** Stream", and it
-- cannot honour that against a column that does not exist. `review_cadence` is
-- the trigger side of the same feature ("triggered by the user's chosen
-- cadence"), so it lands here too rather than in a second migration later.
--
-- All three are projections: rebuildable from the op log, since they ride on
-- the Stream entity inside every stream.create / stream.update op. Existing
-- rows upgrade to the entity's own defaults — not paused, reviewed weekly —
-- which is what a pre-v12 vault effectively already behaved as.

ALTER TABLE streams ADD COLUMN paused INTEGER NOT NULL DEFAULT 0;
ALTER TABLE streams ADD COLUMN paused_until_ms INTEGER;
ALTER TABLE streams ADD COLUMN review_cadence TEXT NOT NULL DEFAULT 'weekly';
