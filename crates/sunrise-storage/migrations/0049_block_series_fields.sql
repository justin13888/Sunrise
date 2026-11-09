-- 0049: a Block's kind, series and import fields (issue #1147,
-- docs/02-domain/time-blocks.md §Fields).
--
-- Why
-- ---
--
-- A Block gains eight optional fields: `kind` (fixed / flexible), the
-- recurring series' `rrule`, `exceptions` and `split_from`, and `location`,
-- `notes`, `source` and `external_id`. Each gets a column, so a read never
-- has to dig it out of `extra`:
--
-- - `kind` and `source` hold the raw spelling, so a value this build does not
--   know is kept verbatim (ADR-0045 §6). NULL is an absent field, which reads
--   by time-blocks.md §Migration default.
-- - `rrule_cbor` and `exceptions` hold canonical CBOR, as
--   `routines.rrule_cbor` does: a rule's raw values can hold any string, and
--   the exceptions are a map of records.
-- - `notes` holds the note body's raw bytes, as `tasks.body` does.
-- - `split_from` is the 16-byte id of the series root.
--
-- NULL for every existing block: no build before this one wrote any of them.
--
-- How
-- ---
--
-- Eight columns appended with `ALTER TABLE … ADD COLUMN`; no row is
-- rewritten. `blocks` is a projection of the op log (ADR-0065 §5). A row a
-- build before this one folded from an op that already carried one of these
-- fields (a newer device's write) holds it in `extra`, as an unknown field,
-- which SQL cannot move out of the CBOR blob. This build's FOLD_V differs from
-- the one `projection_rebuild` records, so the first `Core::open` after this
-- migration rebuilds every row from the ops, which puts each in its column.

ALTER TABLE blocks ADD COLUMN kind TEXT;
ALTER TABLE blocks ADD COLUMN rrule_cbor BLOB;
ALTER TABLE blocks ADD COLUMN exceptions BLOB;
ALTER TABLE blocks ADD COLUMN split_from BLOB;
ALTER TABLE blocks ADD COLUMN location TEXT;
ALTER TABLE blocks ADD COLUMN notes BLOB;
ALTER TABLE blocks ADD COLUMN source TEXT;
ALTER TABLE blocks ADD COLUMN external_id TEXT;
