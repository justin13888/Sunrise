-- 0051: a task bound to one occurrence of a recurring block (issue #1162,
-- docs/02-domain/time-blocks.md §Recurring blocks).
--
-- Why
-- ---
--
-- A binding to a recurring block names one occurrence: the Block's
-- `occurrence_tasks` OR-set holds `[occurrence-key, task]` pairs beside the
-- whole-block `tasks`. `block_tasks` is the one projection of both, and
-- `Task.blocks` is derived from it, so it gains `occurrence_key`:
--
-- - the empty string for a binding to the whole block, which is every
--   existing row: no build before this one wrote an occurrence binding;
-- - the occurrence key (`YYYY-MM-DDTHH:MM`) for a binding to one occurrence.
--
-- The key joins the row's identity: one task may be bound to two
-- occurrences of one block, and to the whole block besides.
--
-- How
-- ---
--
-- SQLite cannot change a primary key in place, so `block_tasks` is rebuilt
-- and every row copied with the empty key. Nothing references `block_tasks`,
-- so dropping it cascades nowhere. The key is NOT NULL with an empty-string
-- default rather than a nullable column, because a NULL in a rowid table's
-- composite PRIMARY KEY is never equal to another NULL, so two copies of one
-- whole-block binding would both be kept. The index on `task_id` is
-- recreated as it was.
--
-- `block_tasks` is a projection of the op log (ADR-0065 §5). An occurrence
-- binding a build before this one folded from a newer device's op is held in
-- the block's `extra`, as an unknown field. This build's FOLD_V differs from
-- the one `projection_rebuild` records, so the first `Core::open` after this
-- migration rebuilds every row from the ops, which puts each in the table.

CREATE TABLE block_tasks_0051 (
    block_id        BLOB NOT NULL REFERENCES blocks (id) ON DELETE CASCADE,
    task_id         BLOB NOT NULL,
    occurrence_key  TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (block_id, task_id, occurrence_key)
);

INSERT INTO block_tasks_0051 (block_id, task_id)
SELECT block_id, task_id FROM block_tasks;

DROP TABLE block_tasks;
ALTER TABLE block_tasks_0051 RENAME TO block_tasks;

CREATE INDEX block_tasks_by_task ON block_tasks (task_id);
