-- Sunrise local storage schema, migration to version 7: the `contexts` table.
-- Per the migrations doc: schema changes are always a NEW migration file,
-- never an edit of an existing one.
--
-- Migration 0001 shipped `task_contexts` (the Task ↔ Context OR-set) but no
-- table for the Context entity itself, so a context could only ever be
-- referenced by an id no user could obtain. This adds the materialized
-- projection backing `Command::CreateContext` / `UpdateContext` / `DeleteContext`
-- and `Query::Contexts`, per docs/02-domain/contexts-and-tags.md.
--
-- Columns mirror the other materialized entity tables (streams, routines):
-- `archived` / `deleted` flags plus the entity-level LWW pair introduced in
-- 0006, so a Context converges under exactly the same rule as everything else.
--
-- The name index is deliberately NOT UNIQUE. Duplicate-name rejection is a
-- *local command* rule (the engine checks before it writes an op); a UNIQUE
-- index would instead make remote materialization fail hard when two replicas
-- concurrently create `@errands` offline, aborting the whole apply transaction
-- and permanently wedging sync. Concurrent same-name creates converge to two
-- rows, which capture reports as an ambiguous `@name` — recoverable, unlike a
-- constraint violation on the receive path.

CREATE TABLE contexts (
    id              BLOB PRIMARY KEY,
    name            TEXT NOT NULL,
    description     TEXT,
    archived        INTEGER NOT NULL DEFAULT 0,
    deleted         INTEGER NOT NULL DEFAULT 0,
    created_at_ms   INTEGER NOT NULL DEFAULT 0,
    updated_at_ms   INTEGER NOT NULL DEFAULT 0,
    lww_ts_ms       INTEGER NOT NULL DEFAULT 0,
    lww_device      BLOB
);

CREATE INDEX contexts_by_name ON contexts (name COLLATE NOCASE) WHERE deleted = 0;
