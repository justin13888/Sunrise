-- 0041: every stored SunriseTime keeps its civil time, and the routine and
-- stream times become SunriseTimes (issue #506, ADR-0066 §1-§2,
-- docs/10-cross-cutting/time.md §1).
--
-- Why
-- ---
--
-- A `zoned` value was stored as its instant (`*_ms`) and its zone name
-- (`*_tz`), and its wall-clock time was re-derived from the instant on every
-- read (`SunriseTime::from_parts`). A tzdata change to the zone's rules then
-- moved a stored 09:00, and a zone name this build could not resolve rebuilt
-- the civil time in UTC. Each SunriseTime column now has a `*_civil` sidecar
-- holding the civil date-time (or date) a `zoned`, `floating` or `all_day`
-- value means, and a read rebuilds the value from it and the zone name alone.
--
-- `Routine.starts_at`, `ends_at`, `paused_until` and `last_completed_at`, and
-- `Stream.paused_until`, were bare instants with no `_kind` or `_tz`. They
-- gain the three sidecars the task and block times have.
--
-- Back-fill
-- ---------
--
-- SQL cannot read tzdata, so no `*_civil` value is computed here. Every one
-- of these tables is a projection of the op log (ADR-0065 §5), and this
-- build's FOLD_V differs from the one `projection_rebuild` records, so the
-- first `Core::open` after this migration rebuilds every row from the ops.
-- The ops carry each civil value as it was written, which is better than any
-- derivation from `*_ms`: a value whose instant a tzdata change has moved is
-- restored to the wall-clock time it was written with. Until then, and for a
-- row the rebuild cannot reach, a NULL `*_civil` reads as before: derived from
-- `*_ms` in the value's zone.
--
-- The routine and stream kinds are back-filled with what those bare instants
-- mean (#506): a routine's anchor and end are `zoned` in the routine's own
-- `timezone`, so an existing routine keeps firing when it did; its pause and
-- last completion, and a stream's pause, are `instant`s.

ALTER TABLE tasks ADD COLUMN scheduled_at_civil TEXT;
ALTER TABLE tasks ADD COLUMN due_at_civil TEXT;
ALTER TABLE tasks ADD COLUMN completed_at_civil TEXT;

ALTER TABLE blocks ADD COLUMN starts_at_civil TEXT;
ALTER TABLE blocks ADD COLUMN ends_at_civil TEXT;

ALTER TABLE routines ADD COLUMN starts_at_kind TEXT NOT NULL DEFAULT 'instant';
ALTER TABLE routines ADD COLUMN starts_at_tz TEXT;
ALTER TABLE routines ADD COLUMN starts_at_civil TEXT;
ALTER TABLE routines ADD COLUMN ends_at_kind TEXT;
ALTER TABLE routines ADD COLUMN ends_at_tz TEXT;
ALTER TABLE routines ADD COLUMN ends_at_civil TEXT;
ALTER TABLE routines ADD COLUMN paused_until_kind TEXT;
ALTER TABLE routines ADD COLUMN paused_until_tz TEXT;
ALTER TABLE routines ADD COLUMN paused_until_civil TEXT;
ALTER TABLE routines ADD COLUMN last_completed_at_kind TEXT;
ALTER TABLE routines ADD COLUMN last_completed_at_tz TEXT;
ALTER TABLE routines ADD COLUMN last_completed_at_civil TEXT;

UPDATE routines SET starts_at_kind = 'zoned', starts_at_tz = timezone;
UPDATE routines SET ends_at_kind = 'zoned', ends_at_tz = timezone
    WHERE ends_at_ms IS NOT NULL;
UPDATE routines SET paused_until_kind = 'instant' WHERE paused_until_ms IS NOT NULL;
UPDATE routines SET last_completed_at_kind = 'instant'
    WHERE last_completed_at_ms IS NOT NULL;

ALTER TABLE streams ADD COLUMN paused_until_kind TEXT;
ALTER TABLE streams ADD COLUMN paused_until_tz TEXT;
ALTER TABLE streams ADD COLUMN paused_until_civil TEXT;

UPDATE streams SET paused_until_kind = 'instant' WHERE paused_until_ms IS NOT NULL;
