-- Sunrise local storage schema, migration to version 10: focus sessions.
-- Per the migrations doc: schema changes are always a NEW migration file,
-- never an edit of an existing one.
--
-- Focus Mode (docs/08-features/focus-mode.md) records wall-clock work against a
-- task. ADR-0013 decides the representation: an APPEND-ONLY record keyed by its
-- own EntityRef (`fcs_`), never a mutable field on `tasks` and never a ticking
-- register. That decision is what these three tables encode.
--
-- Why the start and the end are SEPARATE tables, not one row updated in place:
--
--   * The two ops are independent facts. A `start` with no `end` is a VALID
--     state — "still running", the state the app dying mid-session leaves — and
--     it must read that way without a tombstone or a repair pass.
--   * Ops arrive out of order. If the end lived as an UPDATE to the start row,
--     an `end` that overtook its `start` would update zero rows and be lost
--     from the projection. As its own insert it simply lands, and the session
--     view is the LEFT JOIN of the two. Neither op can clobber the other, in
--     either arrival order.
--   * Both writes are therefore INSERT-only and idempotent on the primary key,
--     which is what makes concurrent sessions on two devices COEXIST AND
--     AGGREGATE (distinct `fcs_` ids => distinct rows) instead of contending
--     over one register the way an LWW field on `tasks` would. That is the
--     property ADR-0013 turns on, and it needs no OR-Set to hold.
--
-- NO foreign keys, for the same reason `task_blockers` (0008) has none: a focus
-- op can be materialized before the `task.create` it names exists on this
-- replica, and an FK would abort the apply transaction and wedge the receive
-- path. A session naming an unknown task is a *fact*; it joins up when the task
-- arrives.
--
-- NOTHING TICKING IS STORED. There is deliberately no `elapsed_ms` column:
-- elapsed is derived on read as `clock.now_ms() - started_at_ms`, and only the
-- frozen `actual_focused_ms` is ever persisted, written once by the `end` op.
--
-- All three tables are projections: rebuildable from the op log, since the same
-- records ride inside every focus.start / focus.end / focus.interrupt op.

-- The `start` op's immutable record.
CREATE TABLE focus_sessions (
    id                 BLOB PRIMARY KEY,   -- the session's own `fcs_` id
    task_id            BLOB NOT NULL,
    stream_id          BLOB NOT NULL,
    started_at_ms      INTEGER NOT NULL,
    planned_ms         INTEGER,            -- NULL = open-ended ("until done")
    energy             TEXT,               -- declared energy budget
    kind               TEXT NOT NULL,      -- 'work' | 'break'
    chunk_index        INTEGER,            -- "chunk N of M" checkpoint,
    chunk_total        INTEGER,            --   NULL when the estimate fits
    lww_ts_ms          INTEGER,
    lww_device         BLOB
);
CREATE INDEX focus_sessions_by_task ON focus_sessions (task_id);
CREATE INDEX focus_sessions_by_stream ON focus_sessions (stream_id, started_at_ms);

-- The `end` op's immutable record. Absent row == session still running.
CREATE TABLE focus_session_ends (
    session_id         BLOB PRIMARY KEY,
    ended_at_ms        INTEGER NOT NULL,
    actual_focused_ms  INTEGER NOT NULL,   -- frozen here and nowhere else
    completed_task     INTEGER NOT NULL DEFAULT 0,
    lww_ts_ms          INTEGER,
    lww_device         BLOB
);

-- Interruptions: a grow-only set. The whole triple is the primary key, so
-- re-delivery is idempotent and two devices logging different interruptions
-- against one session both survive.
CREATE TABLE focus_interruptions (
    session_id         BLOB NOT NULL,
    at_ms              INTEGER NOT NULL,
    reason             TEXT NOT NULL,      -- 'self'|'meeting'|'blocked'|'other'
    PRIMARY KEY (session_id, at_ms, reason)
);
