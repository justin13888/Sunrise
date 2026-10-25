-- 0046: what the triage queue writes on a task (ADR-0047 §3–§4, issue #335,
-- docs/02-domain/tasks.md).
--
-- Why
-- ---
--
-- Two of the queue's four actions record something no column held:
--
-- - `tasks.cancel_reason` holds `Task.cancel_reason`, the optional reason a
--   Drop gives. Meaningful only while the task is cancelled.
-- - `tasks.late_acknowledged_at_ms` holds `Task.late_acknowledged_at`, the
--   moment a Keep was given. A task leaves the queue while it is at or after
--   the moment the task's current lateness began.
--
-- Both are NULL for every existing task: no task has been dropped with a
-- reason or kept before this build. The other two actions write columns that
-- exist: a Defer the plan and the deferral count, an Already done the state and
-- `completed_at`, which already holds an instant or a date.
--
-- A row a build before this one folded from an op that already carried one of
-- them (a newer device's write) holds it in `extra`, as an unknown field. SQL
-- cannot move it out of the CBOR blob. This build's FOLD_V differs from the
-- one `projection_rebuild` records, so the first `Core::open` after this
-- migration rebuilds every row from the ops, which puts the value in its
-- column and takes it out of `extra`.

ALTER TABLE tasks ADD COLUMN cancel_reason TEXT;
ALTER TABLE tasks ADD COLUMN late_acknowledged_at_ms INTEGER;
