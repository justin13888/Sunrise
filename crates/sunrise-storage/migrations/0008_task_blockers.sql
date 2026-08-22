-- Sunrise local storage schema, migration to version 8: the task dependency
-- index. Per the migrations doc: schema changes are always a NEW migration
-- file, never an edit of an existing one.
--
-- Migration 0001 gave `tasks` no blocker columns at all: a Task's `blocked_by`
-- OR-set lived only inside the op-log inner-op CBOR, so the materialized
-- projection dropped it on the floor and `blocked` could never be derived. This
-- adds the edge table that makes BOTH directions cheap:
--
--   * forward  (`blocked_by`)    — PRIMARY KEY (task_id, blocker_id) covers it,
--   * reverse  (`blocks_others`) — the by_blocker index covers it.
--
-- `blocks_others` stays derived-only per docs/02-domain/tasks.md: it is never a
-- column on `tasks` and never rides the wire. Both directions are read out of
-- this one table, which is also what lets a planner rank actionable tasks by
-- how many dependents completing them would release without a full scan.
--
-- NO foreign keys, deliberately. Ops arrive out of order: a `task.update` that
-- names a blocker can be materialized before the `task.create` for that blocker
-- exists on this replica. An FK on `blocker_id` would abort the apply
-- transaction and wedge the receive path; an FK on `task_id` would buy nothing
-- (tasks are soft-deleted, so ON DELETE CASCADE never fires). An edge to an
-- unknown task is a *fact*, and a task whose blocker has not arrived yet counts
-- as blocked until it does — which is exactly the convergent answer.
--
-- The table is a projection: rebuildable from the op log.

CREATE TABLE task_blockers (
    task_id     BLOB NOT NULL,
    blocker_id  BLOB NOT NULL,
    PRIMARY KEY (task_id, blocker_id)
);

CREATE INDEX task_blockers_by_blocker ON task_blockers (blocker_id);
