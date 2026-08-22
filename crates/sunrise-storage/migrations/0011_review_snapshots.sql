-- Sunrise local storage schema, migration to version 11: review snapshots.
-- Per the migrations doc: schema changes are always a NEW migration file,
-- never an edit of an existing one.
--
-- `docs/08-features/reviews-and-stats.md` §Weekly review step 5 ends the flow
-- with "a review snapshot stored as an opaque entity (queryable in History)".
--
-- WHY THIS IS THE *ONLY* NEW TABLE THE REVIEW FEATURE NEEDS.
--
-- Everything else the feature reports — the per-Stream weekly summaries, the
-- completed/deferred trends, the activity timeline, time-in-focus — is a fold
-- over data that already exists. The op log records every mutation with its
-- timestamp, its authoring device and (because v1 ops are full-state) the
-- entity's complete value after the write, so any past week can be recomputed
-- exactly. Persisting those numbers would be a second, staler copy of an
-- answer the log already holds, and a cache that can disagree with its source.
--
-- What the log does NOT hold is the fact that a human sat down and reviewed a
-- given week. That is a new fact, so it is a new op and a new row.
--
-- APPEND-ONLY, keyed by its own `rvw_` id — the same shape ADR-0013 chose for
-- focus sessions, for the same reasons:
--
--   * Two devices can each finish a review of the same week. Distinct ids mean
--     two rows that coexist, not a last-writer-wins register where one user's
--     reflection silently overwrites the other's.
--   * A re-delivered op is an INSERT OR IGNORE on the primary key: idempotent,
--     with no LWW comparison to get wrong.
--   * A snapshot is immutable once written, so there is no update path and no
--     way for a clock skew to rewrite history.
--
-- NO FOREIGN KEYS, for the reason `focus_sessions` (0010) and `task_blockers`
-- (0008) have none: a snapshot op can arrive before the Streams it names have
-- been materialized on this replica, and an FK would abort the receive
-- transaction and wedge sync. The per-Stream counts live inside `body` (an
-- opaque canonical-CBOR blob) precisely so this table never needs to join.
--
-- The table is a projection: rebuildable from the op log, since the whole
-- snapshot rides inside every review.snapshot op.

CREATE TABLE review_snapshots (
    id                 BLOB PRIMARY KEY,   -- the snapshot's own `rvw_` id
    created_at_ms      INTEGER NOT NULL,   -- when the review was completed
    window_start_ms    INTEGER NOT NULL,   -- inclusive start of the reviewed window
    window_end_ms      INTEGER NOT NULL,   -- exclusive end
    completed          INTEGER NOT NULL DEFAULT 0,
    deferred           INTEGER NOT NULL DEFAULT 0,
    dropped            INTEGER NOT NULL DEFAULT 0,
    created            INTEGER NOT NULL DEFAULT 0,
    reopened           INTEGER NOT NULL DEFAULT 0,
    body               BLOB NOT NULL,      -- canonical CBOR: per-stream rows,
                                           --   streaks, and the user's note
    lww_ts_ms          INTEGER,
    lww_device         BLOB
);

-- History reads newest-first, and "which weeks have I reviewed?" reads by
-- window. Both are covered by one index on the window start.
CREATE INDEX review_snapshots_by_window
    ON review_snapshots (window_start_ms DESC, created_at_ms DESC);
