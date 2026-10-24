-- 0042: a task's place in a hand-arranged list, `Task.sort_order` (issue
-- #340, docs/02-domain/tasks.md).
--
-- Why
-- ---
--
-- A hand-arranged task order lived in one client's own store (the Apple
-- client's `UserDefaults`), because a Task had no field to write it to. So an
-- arrangement made on one device never reached another. `Task.sort_order` is
-- a fractional-index key in the encoding `Stream.sort_order` already uses
-- (`sunrise_domain::sort_order`), one register per task, written by
-- `Command::MoveTasks`.
--
-- NULL means "never placed", and is distinct from every key: a list in manual
-- order shows the placed tasks first, by key and then by id, and the unplaced
-- ones after them (`sunrise_domain::task_order`). Unlike 0014 for streams
-- there is no back-fill. Nothing ordered tasks before this, so there is no
-- displayed order to preserve, and inventing one would write a key no op
-- carries, which a rebuild from the log would then take away (migrations.md
-- §Migration rigor rule 1).
--
-- A row a build before this one folded from an op that already carried a
-- `sort_order` (a newer device's write) holds it in `extra`, as an unknown
-- field. SQL cannot move it out of the CBOR blob. This build's FOLD_V differs
-- from the one `projection_rebuild` records, so the first `Core::open` after
-- this migration rebuilds every row from the ops, which puts the key in this
-- column and takes it out of `extra`.
--
-- The index serves the one ordered read, a stream's tasks in manual order.

ALTER TABLE tasks ADD COLUMN sort_order TEXT;

CREATE INDEX tasks_by_stream_sort_order ON tasks (stream_id, sort_order)
    WHERE sort_order IS NOT NULL;
