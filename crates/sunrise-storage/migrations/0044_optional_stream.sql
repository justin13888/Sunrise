-- 0044: a task, block or routine template may belong to no stream; the Inbox
-- is a view, not a stream; and a deleted stream records where it re-homed
-- what it held (ADR-0046, issue #332, docs/02-domain/tasks.md).
--
-- Why
-- ---
--
-- "No stream" was a sentinel Stream with a fixed id (three zero bytes then
-- ASCII `sunrise.inbox`), and `tasks.stream_id`, `blocks.stream_id` and
-- `routines.stream_id` were `NOT NULL REFERENCES streams`. ADR-0046 makes the
-- stream optional and keeps those bytes as the id of the private key domain,
-- which is not a Stream and has no `streams` row. So:
--
-- - the three columns become nullable, without the foreign key, and every
--   value naming the sentinel becomes NULL;
-- - the sentinel's synthetic `streams` row (0017 inserted one; `ensure_stream_row`
--   made one for any vault that ever held an Inbox task) is deleted;
-- - `tasks.triaged_at_ms` holds `Task.triaged_at`, the one explicit bit of the
--   Inbox predicate (ADR-0046 §3). NULL for every existing task, so exactly
--   the tasks that were in the Inbox are in it after, less the ones with a
--   plan or a deadline, which the Inbox never considered before;
-- - `streams.rehomed_to` holds `Stream.rehomed_to`, the target a deletion
--   moved the stream's dependents to (ADR-0046 §4). NULL for every existing
--   stream: a tombstone written before this build re-homed nothing, and reads
--   as a detach.
--
-- How
-- ---
--
-- SQLite cannot drop a NOT NULL or a REFERENCES clause in place, so the three
-- tables are rebuilt: create the new shape, copy, drop the old, rename. The
-- migration runs in one transaction with `foreign_keys` on, which cannot be
-- turned off inside it, and dropping a table with `foreign_keys` on deletes
-- its rows first, which cascades to `task_contexts` and `block_tasks` (`ON
-- DELETE CASCADE`). Both are copied aside first and copied back after, so no
-- row is lost. No other table names these three in a foreign key.
--
-- Every column keeps its name, type, default and position; the new columns
-- are appended. Every index the three tables had is recreated as it was.
--
-- These tables are projections of the op log (ADR-0065 §5), and this build's
-- FOLD_V (6) differs from the one `projection_rebuild` records, so the first
-- `Core::open` after this migration also rebuilds every row from the ops. The
-- rewrite here is what that rebuild would produce for these columns, so a row
-- the rebuild cannot reach already holds it.

-- --- the bindings, set aside from the cascade ---
CREATE TABLE task_contexts_0044 AS SELECT task_id, context_id FROM task_contexts;
CREATE TABLE block_tasks_0044 AS SELECT block_id, task_id FROM block_tasks;

-- --- tasks ---
CREATE TABLE tasks_0044 (
    id                     BLOB PRIMARY KEY,
    -- NULL for a task that belongs to no stream (ADR-0046 §1). The projection
    -- holds the task's effective stream: a deleted stream it names reads as
    -- where that deletion re-homed it. No foreign key: the private domain has
    -- no row, and an entity may name a stream whose create has not arrived.
    stream_id              BLOB,
    title                  TEXT NOT NULL,
    state                  TEXT NOT NULL,
    priority               INTEGER,
    energy                 TEXT,
    estimated_min          INTEGER,
    scheduled_at_ms        INTEGER,
    scheduled_at_kind      TEXT,
    scheduled_at_tz        TEXT,
    due_at_ms              INTEGER,
    due_at_kind            TEXT,
    due_at_tz              TEXT,
    completed_at_ms        INTEGER,
    completed_at_kind      TEXT,
    completed_at_tz        TEXT,
    deferred_count         INTEGER NOT NULL DEFAULT 0,
    routine_id             BLOB,
    routine_occurrence     INTEGER,
    reminder_lead_s        INTEGER,
    archived               INTEGER NOT NULL DEFAULT 0,
    deleted                INTEGER NOT NULL DEFAULT 0,
    body                   BLOB,
    extra                  BLOB,
    head_root              BLOB,
    scheduling_constraints BLOB,
    created_at_ms          INTEGER NOT NULL DEFAULT 0,
    updated_at_ms          INTEGER NOT NULL DEFAULT 0,
    lww_hlc_ms             INTEGER NOT NULL DEFAULT 0,
    lww_hlc_logical        INTEGER NOT NULL DEFAULT 0,
    lww_seq                INTEGER NOT NULL DEFAULT 0,
    lww_device             BLOB,
    scheduled_at_civil     TEXT,
    due_at_civil           TEXT,
    completed_at_civil     TEXT,
    sort_order             TEXT,
    -- `Task.triaged_at` in epoch milliseconds; NULL for untriaged.
    triaged_at_ms          INTEGER
);
INSERT INTO tasks_0044
    (id, stream_id, title, state, priority, energy, estimated_min,
     scheduled_at_ms, scheduled_at_kind, scheduled_at_tz,
     due_at_ms, due_at_kind, due_at_tz,
     completed_at_ms, completed_at_kind, completed_at_tz,
     deferred_count, routine_id, routine_occurrence, reminder_lead_s, archived, deleted,
     body, extra, head_root, scheduling_constraints, created_at_ms, updated_at_ms,
     lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device,
     scheduled_at_civil, due_at_civil, completed_at_civil, sort_order, triaged_at_ms)
SELECT id,
       CASE WHEN stream_id IN (X'00000073756E726973652E696E626F78',
                               X'00000000000000000000000000000000')
            THEN NULL ELSE stream_id END,
       title, state, priority, energy, estimated_min,
       scheduled_at_ms, scheduled_at_kind, scheduled_at_tz,
       due_at_ms, due_at_kind, due_at_tz,
       completed_at_ms, completed_at_kind, completed_at_tz,
       deferred_count, routine_id, routine_occurrence, reminder_lead_s, archived, deleted,
       body, extra, head_root, scheduling_constraints, created_at_ms, updated_at_ms,
       lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device,
       scheduled_at_civil, due_at_civil, completed_at_civil, sort_order, NULL
FROM tasks;
DROP TABLE tasks;
ALTER TABLE tasks_0044 RENAME TO tasks;
CREATE INDEX tasks_by_stream ON tasks (stream_id);
CREATE INDEX tasks_by_due ON tasks (due_at_ms) WHERE due_at_ms IS NOT NULL;
CREATE INDEX tasks_by_stream_sort_order ON tasks (stream_id, sort_order)
    WHERE sort_order IS NOT NULL;

-- --- routines ---
CREATE TABLE routines_0044 (
    id                     BLOB PRIMARY KEY,
    -- The template's stream; NULL for occurrences that belong to no stream.
    stream_id              BLOB,
    rrule_text             TEXT NOT NULL DEFAULT '',
    timezone               TEXT NOT NULL,
    starts_at_ms           INTEGER NOT NULL,
    ends_at_ms             INTEGER,
    template               BLOB,
    skip_dates             BLOB,
    skipped_keys           BLOB,
    catchup_policy         TEXT NOT NULL DEFAULT 'skip',
    streak_counter         INTEGER NOT NULL DEFAULT 0,
    streak_state           BLOB,
    last_completed_at_ms   INTEGER,
    materialized_until_ms  INTEGER NOT NULL DEFAULT 0,
    paused                 INTEGER NOT NULL DEFAULT 0,
    paused_until_ms        INTEGER,
    archived               INTEGER NOT NULL DEFAULT 0,
    deleted                INTEGER NOT NULL DEFAULT 0,
    scheduling_constraints BLOB,
    created_at_ms          INTEGER NOT NULL DEFAULT 0,
    updated_at_ms          INTEGER NOT NULL DEFAULT 0,
    lww_hlc_ms             INTEGER NOT NULL DEFAULT 0,
    lww_hlc_logical        INTEGER NOT NULL DEFAULT 0,
    lww_seq                INTEGER NOT NULL DEFAULT 0,
    lww_device             BLOB,
    extra                  BLOB,
    rrule_cbor             BLOB,
    starts_at_kind         TEXT NOT NULL DEFAULT 'instant',
    starts_at_tz           TEXT,
    starts_at_civil        TEXT,
    ends_at_kind           TEXT,
    ends_at_tz             TEXT,
    ends_at_civil          TEXT,
    paused_until_kind      TEXT,
    paused_until_tz        TEXT,
    paused_until_civil     TEXT,
    last_completed_at_kind TEXT,
    last_completed_at_tz   TEXT,
    last_completed_at_civil TEXT
);
INSERT INTO routines_0044
    (id, stream_id, rrule_text, timezone, starts_at_ms, ends_at_ms, template, skip_dates,
     skipped_keys, catchup_policy, streak_counter, streak_state, last_completed_at_ms,
     materialized_until_ms, paused, paused_until_ms, archived, deleted,
     scheduling_constraints, created_at_ms, updated_at_ms,
     lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device, extra, rrule_cbor,
     starts_at_kind, starts_at_tz, starts_at_civil, ends_at_kind, ends_at_tz, ends_at_civil,
     paused_until_kind, paused_until_tz, paused_until_civil,
     last_completed_at_kind, last_completed_at_tz, last_completed_at_civil)
SELECT id,
       CASE WHEN stream_id IN (X'00000073756E726973652E696E626F78',
                               X'00000000000000000000000000000000')
            THEN NULL ELSE stream_id END,
       rrule_text, timezone, starts_at_ms, ends_at_ms, template, skip_dates,
       skipped_keys, catchup_policy, streak_counter, streak_state, last_completed_at_ms,
       materialized_until_ms, paused, paused_until_ms, archived, deleted,
       scheduling_constraints, created_at_ms, updated_at_ms,
       lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device, extra, rrule_cbor,
       starts_at_kind, starts_at_tz, starts_at_civil, ends_at_kind, ends_at_tz, ends_at_civil,
       paused_until_kind, paused_until_tz, paused_until_civil,
       last_completed_at_kind, last_completed_at_tz, last_completed_at_civil
FROM routines;
DROP TABLE routines;
ALTER TABLE routines_0044 RENAME TO routines;

-- --- blocks ---
CREATE TABLE blocks_0044 (
    id               BLOB PRIMARY KEY,
    -- NULL for a block that belongs to no stream, drawn in the neutral colour.
    stream_id        BLOB,
    starts_at_ms     INTEGER NOT NULL,
    starts_at_kind   TEXT NOT NULL DEFAULT 'instant',
    starts_at_tz     TEXT,
    ends_at_ms       INTEGER NOT NULL,
    ends_at_kind     TEXT NOT NULL DEFAULT 'instant',
    ends_at_tz       TEXT,
    title            TEXT,
    title_track_task INTEGER NOT NULL DEFAULT 0,
    deleted          INTEGER NOT NULL DEFAULT 0,
    extra            BLOB,
    created_at_ms    INTEGER NOT NULL DEFAULT 0,
    updated_at_ms    INTEGER NOT NULL DEFAULT 0,
    lww_hlc_ms       INTEGER NOT NULL DEFAULT 0,
    lww_hlc_logical  INTEGER NOT NULL DEFAULT 0,
    lww_seq          INTEGER NOT NULL DEFAULT 0,
    lww_device       BLOB,
    starts_at_civil  TEXT,
    ends_at_civil    TEXT
);
INSERT INTO blocks_0044
    (id, stream_id, starts_at_ms, starts_at_kind, starts_at_tz, ends_at_ms, ends_at_kind,
     ends_at_tz, title, title_track_task, deleted, extra, created_at_ms, updated_at_ms,
     lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device, starts_at_civil, ends_at_civil)
SELECT id,
       CASE WHEN stream_id IN (X'00000073756E726973652E696E626F78',
                               X'00000000000000000000000000000000')
            THEN NULL ELSE stream_id END,
       starts_at_ms, starts_at_kind, starts_at_tz, ends_at_ms, ends_at_kind,
       ends_at_tz, title, title_track_task, deleted, extra, created_at_ms, updated_at_ms,
       lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device, starts_at_civil, ends_at_civil
FROM blocks;
DROP TABLE blocks;
ALTER TABLE blocks_0044 RENAME TO blocks;
CREATE INDEX blocks_by_time ON blocks (starts_at_ms);
CREATE INDEX blocks_by_end ON blocks (ends_at_ms);

-- --- the bindings, back ---
INSERT OR IGNORE INTO task_contexts (task_id, context_id)
    SELECT task_id, context_id FROM task_contexts_0044
    WHERE task_id IN (SELECT id FROM tasks);
INSERT OR IGNORE INTO block_tasks (block_id, task_id)
    SELECT block_id, task_id FROM block_tasks_0044
    WHERE block_id IN (SELECT id FROM blocks);
DROP TABLE task_contexts_0044;
DROP TABLE block_tasks_0044;

-- --- streams ---
ALTER TABLE streams ADD COLUMN rehomed_to BLOB;
-- The sentinel's synthetic row. A stream that named it as its parent (none
-- could be made, but a forged op could) becomes top-level.
UPDATE streams SET parent_id = NULL WHERE parent_id = X'00000073756E726973652E696E626F78';
DELETE FROM streams WHERE stream_id = X'00000073756E726973652E696E626F78';
