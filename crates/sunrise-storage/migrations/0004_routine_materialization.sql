-- Sunrise local storage schema, migration to version 4.
-- Fills out the `routines` projection so the recurrence engine can persist and
-- re-read routines and drive deterministic task materialization.
-- Per the migrations doc: schema changes are always a NEW migration file,
-- never an edit of an existing one.
--
-- Reconciliation with earlier migrations. The `routines` table from 0001
-- already provides: id, stream_id, rrule (TEXT NOT NULL), timezone
-- (TEXT NOT NULL), starts_at_ms, ends_at_ms, streak_counter, paused, archived,
-- deleted, extra; 0003 added scheduling_constraints. Those columns are reused
-- and are NOT re-added here.
--
-- The legacy 0001 `rrule` TEXT column and `extra` BLOB become effectively
-- unused by the engine. `rrule` is NOT NULL, so on insert the engine writes the
-- canonical RRULE string into BOTH `rrule` (to satisfy the constraint) and the
-- new `rrule_text`; reads use `rrule_text`. `extra` is left NULL.
--
-- All new columns are projections (rebuildable from the op log). Blob columns
-- hold canonical CBOR; `template`/`skip_dates`/`skipped_keys` are opaque to SQL.

ALTER TABLE routines ADD COLUMN template BLOB;
ALTER TABLE routines ADD COLUMN skip_dates BLOB;
ALTER TABLE routines ADD COLUMN skipped_keys BLOB;
ALTER TABLE routines ADD COLUMN catchup_policy TEXT NOT NULL DEFAULT 'skip';
ALTER TABLE routines ADD COLUMN last_completed_at_ms INTEGER;
ALTER TABLE routines ADD COLUMN paused_until_ms INTEGER;
ALTER TABLE routines ADD COLUMN created_at_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE routines ADD COLUMN updated_at_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE routines ADD COLUMN rrule_text TEXT NOT NULL DEFAULT '';
ALTER TABLE routines ADD COLUMN materialized_until_ms INTEGER NOT NULL DEFAULT 0;
