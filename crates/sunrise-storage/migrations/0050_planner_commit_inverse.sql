-- 0050: the planner keeps each commit's inverse (issue #1158,
-- `docs/08-features/planner.md` §Undo, ADR-0048 §7).
--
-- Why
-- ---
--
-- `plan_undo` restores a planner commit per field, and only the fields whose
-- register still holds the commit's write. So the undo needs, for every field
-- the commit wrote, the value it replaced and the stamp it wrote with. The
-- preview cache is in memory and evicts, so the inverse is stored here, with
-- the commit, in the local database.
--
-- Local-only state: no op carries it, a projection rebuild keeps it, and no
-- other device reads it. A commit row is deleted when it is undone, and the
-- oldest are deleted once more than the undo history's bound are held
-- (`PLAN_UNDO_HISTORY` in `sunrise-core`).
--
-- Tables
-- ------
--
-- `planner_commits` holds one row per commit. `commit_id` is the
-- `PlanCommitId` a client holds; `n` orders the commits, oldest first, for
-- the bound.
--
-- `planner_commit_fields` holds one row per field the commit wrote.
-- `entity_id` is the task's or block's 16 bytes, and `field` the field's
-- register name (`scheduled_at`, `starts_at`, `ends_at`). `prior_*` is the
-- value the commit replaced, as the four columns every time is stored as
-- (`tasks.planned_at_*`): `prior_ms` NULL means no value. `hlc_ms`,
-- `hlc_logical`, `device`, `seq` and `stream` are the stamp the commit's write
-- left in `merge_registers`. `undefer` is 1 when the commit was a deferral, so
-- restoring the field also takes one off the task's `deferred_count`.
--
-- Additive: two new tables, empty, and no existing row is touched.
CREATE TABLE planner_commits (
    n             INTEGER PRIMARY KEY,
    commit_id     BLOB NOT NULL UNIQUE,
    committed_ms  INTEGER NOT NULL
);

CREATE TABLE planner_commit_fields (
    commit_id    BLOB NOT NULL REFERENCES planner_commits (commit_id) ON DELETE CASCADE,
    entity_id    BLOB NOT NULL,
    field        TEXT NOT NULL,
    prior_ms     INTEGER,
    prior_kind   TEXT,
    prior_tz     TEXT,
    prior_civil  TEXT,
    hlc_ms       INTEGER NOT NULL,
    hlc_logical  INTEGER NOT NULL,
    device       BLOB NOT NULL,
    seq          INTEGER NOT NULL,
    stream       BLOB NOT NULL,
    undefer      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (commit_id, entity_id, field)
);
