-- Sunrise local storage schema, migration to version 3.
-- Adds the scheduling-constraints projection column to `tasks` and `routines`.
-- Per the migrations doc: schema changes are always a NEW migration file,
-- never an edit of an existing one.
--
-- The column holds a canonical-CBOR blob of the constraint list, or NULL when
-- the list is empty. It is a projection (rebuildable from the op log) and is
-- opaque to SQL in v1.

ALTER TABLE tasks ADD COLUMN scheduling_constraints BLOB;
ALTER TABLE routines ADD COLUMN scheduling_constraints BLOB;
