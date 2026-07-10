-- Sunrise local storage schema, migration to version 2.
-- Adds persisted display name and color to the materialized `streams` table.
-- Per the migrations doc: schema changes are always a NEW migration file,
-- never an edit of an existing one.

ALTER TABLE streams ADD COLUMN name TEXT NOT NULL DEFAULT '';
ALTER TABLE streams ADD COLUMN color TEXT NOT NULL DEFAULT 'slate';
