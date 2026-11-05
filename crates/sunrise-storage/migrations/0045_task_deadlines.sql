-- 0045: a task has a plan and two deadlines (ADR-0047 §1, issue #334,
-- docs/02-domain/tasks.md).
--
-- Why
-- ---
--
-- Two times carried three meanings: `scheduled_at` was both "when I intend
-- to do it" and, in places, the target, and `due_at` was the only thing that
-- could make a task late. ADR-0047 splits them:
--
-- - `planned_at` is the plan, owned by the planner and defer;
-- - `target_at` is the soft deadline;
-- - `hard_due_at` is the hard deadline.
--
-- The migration maps `scheduled_at` to `planned_at` and `due_at` to
-- `hard_due_at`, value and kind unchanged, and gives no task a `target_at`.
-- Each of the four column groups (`_ms` index key, `_kind`, `_tz`, `_civil`
-- sidecars, ADR-0066 §2) is renamed in place, so every row keeps its value
-- byte for byte and no derivation runs.
--
-- The ops keep the wire keys `scheduled_at` and `due_at`, which every build
-- reads with these meanings (ADR-0047 clarification C1); only the storage
-- names move.
--
-- A row a build before this one folded from an op that already carried a
-- `target_at` (a newer device's write) holds it in `extra`, as an unknown
-- field. SQL cannot move it out of the CBOR blob. This build's FOLD_V differs
-- from the one `projection_rebuild` records, so the first `Core::open` after
-- this migration rebuilds every row from the ops, which puts the target in
-- its column and takes it out of `extra`.
--
-- How
-- ---
--
-- `ALTER TABLE … RENAME COLUMN` rewrites every index that names the column,
-- so `tasks_by_due` keeps its name and now indexes `hard_due_at_ms`. The
-- target gets an index of the same shape: lateness and the date operators
-- prefilter on it (`docs/10-cross-cutting/time.md` §2 rule 2).

ALTER TABLE tasks RENAME COLUMN scheduled_at_ms TO planned_at_ms;
ALTER TABLE tasks RENAME COLUMN scheduled_at_kind TO planned_at_kind;
ALTER TABLE tasks RENAME COLUMN scheduled_at_tz TO planned_at_tz;
ALTER TABLE tasks RENAME COLUMN scheduled_at_civil TO planned_at_civil;

ALTER TABLE tasks RENAME COLUMN due_at_ms TO hard_due_at_ms;
ALTER TABLE tasks RENAME COLUMN due_at_kind TO hard_due_at_kind;
ALTER TABLE tasks RENAME COLUMN due_at_tz TO hard_due_at_tz;
ALTER TABLE tasks RENAME COLUMN due_at_civil TO hard_due_at_civil;

ALTER TABLE tasks ADD COLUMN target_at_ms INTEGER;
ALTER TABLE tasks ADD COLUMN target_at_kind TEXT;
ALTER TABLE tasks ADD COLUMN target_at_tz TEXT;
ALTER TABLE tasks ADD COLUMN target_at_civil TEXT;

CREATE INDEX tasks_by_target ON tasks (target_at_ms) WHERE target_at_ms IS NOT NULL;
