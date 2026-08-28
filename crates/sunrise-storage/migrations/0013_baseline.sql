-- Sunrise local storage schema — BASELINE, STORAGE_V = 13.
--
-- This file is the whole schema. Migrations 0001..0012 were collapsed into it
-- by the pre-1.0 storage reset (ADR-0018): twelve incremental files describing
-- a schema no shipped vault has ever held, three of whose columns no code has
-- ever read. A vault stamped `0 < storage_v < 13` is REFUSED, not upgraded —
-- see `Db::ensure_schema` and `DbError::StorageVPreBaseline`. There is no
-- upgrade path across the reset because there is no vault to carry across it.
--
-- The precedent is the header of the old `0005_sync_local.sql`, which already
-- told developers to wipe dev vaults rather than convert them. This does the
-- same thing, once, explicitly, and with a typed error instead of a comment.
--
-- WHAT WAS DELIBERATELY DROPPED (present in 0001..0012, absent here):
--
--   * `streams.doc_blob` / `streams.doc_blob_v` — the Loro CRDT document.
--     ADR-0014 replaced CRDT merge with entity-level LWW; the columns survived
--     as an empty blob and a literal `1` written on every single stream insert.
--   * `merge_journal` — a per-FIELD conflict journal for a merge model that is
--     entity-level. Zero writers, zero readers.
--   * `routines.rrule` — superseded by `rrule_text` in 0004, but left
--     `NOT NULL`, so the engine wrote the same string into both columns
--     forever to satisfy a constraint on a column nothing read.
--   * `routines.extra` — 0004 recorded it as "left NULL"; it still is.
--     Forward-compat unknowns are carried on `tasks.extra`, which IS read.
--
-- After 1.0 this file freezes and the append-only rule in
-- `docs/04-storage/migrations.md` resumes: every change is a new file.
--
-- Pragmas are applied programmatically on connection open, not here. See
-- db.rs::Db::apply_pragmas.

-- --- meta ---
CREATE TABLE schema_meta (
    storage_v       INTEGER NOT NULL,
    applied_at_ms   INTEGER NOT NULL
);
INSERT INTO schema_meta (storage_v, applied_at_ms) VALUES (13, 0);

-- --- op log ---
CREATE TABLE ops (
    op_id           BLOB PRIMARY KEY,
    stream_id       BLOB NOT NULL,
    device_id       BLOB NOT NULL,
    seq             INTEGER NOT NULL,
    ts_ms           INTEGER NOT NULL,
    envelope        BLOB NOT NULL,
    inner_kind      TEXT NOT NULL,
    target_kind     TEXT NOT NULL,
    target_id       BLOB,
    applied_at      INTEGER,
    received_from   BLOB,
    received_at     INTEGER NOT NULL,
    UNIQUE (stream_id, device_id, seq)
);
CREATE INDEX ops_by_stream ON ops (stream_id, seq);
CREATE INDEX ops_unapplied ON ops (applied_at) WHERE applied_at IS NULL;

CREATE TABLE op_dep (
    op_id           BLOB NOT NULL,
    dep_id          BLOB NOT NULL,
    PRIMARY KEY (op_id, dep_id)
);
CREATE INDEX op_dep_reverse ON op_dep (dep_id);

-- --- the local device identity (id pinned to 1) ---
-- `signing_secret_wrapped` is the device Ed25519 signing seed sealed under the
-- vault root with XChaCha20-Poly1305 (AAD = "sunrise.local_identity.v1" ||
-- device_id). `cert_blob` is the self-issued DeviceCert (canonical CBOR).
CREATE TABLE local_identity (
    id                      INTEGER PRIMARY KEY CHECK (id = 1),
    device_id               BLOB NOT NULL,
    signing_secret_wrapped  BLOB NOT NULL,
    cert_blob               BLOB NOT NULL,
    created_at_ms           INTEGER NOT NULL
);

-- Ops awaiting push to peers. `acked_at_ms IS NULL` = still pending.
CREATE TABLE outbox (
    op_id           BLOB PRIMARY KEY REFERENCES ops (op_id),
    stream_id       BLOB NOT NULL,
    enqueued_at_ms  INTEGER NOT NULL,
    acked_at_ms     INTEGER
);
CREATE INDEX outbox_unacked ON outbox (enqueued_at_ms) WHERE acked_at_ms IS NULL;

-- Highest applied seq per (stream_id, device_id), for gap detection and pull
-- resumption.
CREATE TABLE sync_cursors (
    stream_id         BLOB NOT NULL,
    device_id         BLOB NOT NULL,
    last_applied_seq  INTEGER NOT NULL,
    PRIMARY KEY (stream_id, device_id)
);

-- --- streams (materialized projection) ---
-- The entity-level LWW stamp decides which of two concurrent writes to a row
-- survives. It is the triple `(lww_hlc_ms, lww_hlc_logical)`, `lww_device`,
-- `lww_seq`, compared in that order — a hybrid logical clock, then the raw
-- 16-byte device id (memcmp, higher wins), then the writer's per-(stream,
-- device) sequence number. See docs/05-sync/conflict-resolution.md and
-- ADR-0016. `lww_device` is NULL only for a placeholder row no real op has
-- stamped yet (the lazily created inbox/meta stream).
CREATE TABLE streams (
    stream_id       BLOB PRIMARY KEY,
    head_root       BLOB NOT NULL,
    last_op_seq     INTEGER NOT NULL,
    parent_id       BLOB REFERENCES streams (stream_id),
    name            TEXT NOT NULL DEFAULT '',
    color           TEXT NOT NULL DEFAULT 'slate',
    icon            TEXT,
    archived        INTEGER NOT NULL DEFAULT 0,
    deleted         INTEGER NOT NULL DEFAULT 0,
    paused          INTEGER NOT NULL DEFAULT 0,
    paused_until_ms INTEGER,
    review_cadence  TEXT NOT NULL DEFAULT 'weekly',
    created_at_ms   INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL,
    lww_hlc_ms       INTEGER NOT NULL DEFAULT 0,
    lww_hlc_logical  INTEGER NOT NULL DEFAULT 0,
    lww_seq          INTEGER NOT NULL DEFAULT 0,
    lww_device       BLOB
);

-- --- tasks (materialized projection) ---
-- `extra` carries forward-compat unknowns: fields written by a newer
-- DOC_SCHEMA_V that this build does not model, preserved verbatim so a
-- round-trip through an older client does not destroy them.
--
-- SCHEDULED / DUE / COMPLETED are `SunriseTime` values (issue #6), each
-- projected onto THREE columns:
--
--   *_at_ms    the epoch-millisecond INDEX key. Every range query and every
--              ORDER BY in the engine reads this and nothing else, which is
--              why adding zoned/floating/all-day kinds did not touch a single
--              query. For an instant or a zoned time it is the true instant;
--              for a floating time or an all-day date — which have no instant
--              until a reader supplies a zone — it is the UTC anchoring, which
--              is stable across devices and lossless.
--   *_at_kind  'instant' | 'zoned' | 'floating' | 'all_day'. NULL for a NULL
--              value; an UNRECOGNISED value degrades to 'instant' on read
--              rather than failing the row.
--   *_at_tz    the IANA zone name, non-NULL only for 'zoned'.
--
-- The kind cannot be inferred from the index key, and losing it is what made
-- "sometime Tuesday morning" arrive on Monday evening for anyone west of UTC.
CREATE TABLE tasks (
    id                     BLOB PRIMARY KEY,
    stream_id              BLOB NOT NULL REFERENCES streams (stream_id),
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
    lww_device             BLOB
);
CREATE INDEX tasks_by_stream ON tasks (stream_id);
CREATE INDEX tasks_by_due ON tasks (due_at_ms) WHERE due_at_ms IS NOT NULL;

-- Task <-> Context membership (an OR-set on the entity).
CREATE TABLE task_contexts (
    task_id     BLOB NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    context_id  BLOB NOT NULL,
    PRIMARY KEY (task_id, context_id)
);
CREATE INDEX task_contexts_by_context ON task_contexts (context_id);

-- The task dependency index. BOTH directions come out of this one table:
-- forward (`blocked_by`) from the primary key, reverse (`blocks_others`) from
-- the by_blocker index. `blocks_others` is derived-only per
-- docs/02-domain/tasks.md — never a column, never on the wire.
--
-- NO foreign keys, deliberately. Ops arrive out of order: a `task.update`
-- naming a blocker can be materialized before that blocker's `task.create`
-- reaches this replica. An FK would abort the apply transaction and wedge the
-- receive path. An edge to an unknown task is a FACT, and a task whose blocker
-- has not arrived counts as blocked until it does — the convergent answer.
CREATE TABLE task_blockers (
    task_id     BLOB NOT NULL,
    blocker_id  BLOB NOT NULL,
    PRIMARY KEY (task_id, blocker_id)
);
CREATE INDEX task_blockers_by_blocker ON task_blockers (blocker_id);

-- --- contexts (materialized projection) ---
-- The name index is deliberately NOT UNIQUE. Duplicate-name rejection is a
-- LOCAL COMMAND rule, checked before an op is written. A UNIQUE index would
-- instead make remote materialization fail hard when two replicas concurrently
-- create `@errands` offline, aborting the apply transaction and permanently
-- wedging sync. Concurrent same-name creates converge to two rows, which
-- capture reports as an ambiguous `@name` — recoverable, unlike a constraint
-- violation on the receive path.
CREATE TABLE contexts (
    id              BLOB PRIMARY KEY,
    name            TEXT NOT NULL,
    description     TEXT,
    archived        INTEGER NOT NULL DEFAULT 0,
    deleted         INTEGER NOT NULL DEFAULT 0,
    created_at_ms   INTEGER NOT NULL DEFAULT 0,
    updated_at_ms   INTEGER NOT NULL DEFAULT 0,
    lww_hlc_ms       INTEGER NOT NULL DEFAULT 0,
    lww_hlc_logical  INTEGER NOT NULL DEFAULT 0,
    lww_seq          INTEGER NOT NULL DEFAULT 0,
    lww_device       BLOB
);
CREATE INDEX contexts_by_name ON contexts (name COLLATE NOCASE) WHERE deleted = 0;

-- --- routines (materialized projection) ---
-- Blob columns hold canonical CBOR and are opaque to SQL. `streak_state` folds
-- the grace window, the forgiveness toggle and its consumed count, the streak
-- anchor, and the idempotency-key set into one blob: the engine reads them as a
-- unit whenever a Routine is materialized and never queries them from SQL.
-- NULL means "all defaults" — the state of a routine never completed.
CREATE TABLE routines (
    id                     BLOB PRIMARY KEY,
    stream_id              BLOB NOT NULL REFERENCES streams (stream_id),
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
    lww_device             BLOB
);

-- --- time blocks ---
-- Block bounds are `SunriseTime` values too, with the same three-column
-- projection as the task times above. A 09:00 block is a different commitment
-- from a block at a fixed instant, and flying to another timezone must move
-- one and not the other.
CREATE TABLE blocks (
    id               BLOB PRIMARY KEY,
    stream_id        BLOB NOT NULL REFERENCES streams (stream_id),
    starts_at_ms     INTEGER NOT NULL,
    starts_at_kind   TEXT NOT NULL DEFAULT 'instant',
    starts_at_tz     TEXT,
    ends_at_ms       INTEGER NOT NULL,
    ends_at_kind     TEXT NOT NULL DEFAULT 'instant',
    ends_at_tz       TEXT,
    title            TEXT,
    deleted          INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX blocks_by_time ON blocks (starts_at_ms);

CREATE TABLE block_tasks (
    block_id        BLOB NOT NULL REFERENCES blocks (id) ON DELETE CASCADE,
    task_id         BLOB NOT NULL,
    PRIMARY KEY (block_id, task_id)
);

-- --- focus sessions (ADR-0013) ---
-- The start and the end are SEPARATE tables, not one row updated in place:
--
--   * A `start` with no `end` is a VALID state — "still running", the state the
--     app dying mid-session leaves — and must read that way with no tombstone.
--   * Ops arrive out of order. As an UPDATE, an `end` that overtook its `start`
--     would update zero rows and be lost. As its own INSERT it simply lands,
--     and a session is the LEFT JOIN of the two.
--   * Both writes are INSERT-only and idempotent on the primary key, so
--     concurrent sessions on two devices COEXIST AND AGGREGATE instead of
--     contending over one register.
--
-- NOTHING TICKING IS STORED: elapsed is derived on read; only the frozen
-- `actual_focused_ms` is persisted, written once by the `end` op.
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
    lww_hlc_ms         INTEGER NOT NULL DEFAULT 0,
    lww_hlc_logical    INTEGER NOT NULL DEFAULT 0,
    lww_seq            INTEGER NOT NULL DEFAULT 0,
    lww_device         BLOB
);
CREATE INDEX focus_sessions_by_task ON focus_sessions (task_id);
CREATE INDEX focus_sessions_by_stream ON focus_sessions (stream_id, started_at_ms);

CREATE TABLE focus_session_ends (
    session_id         BLOB PRIMARY KEY,
    ended_at_ms        INTEGER NOT NULL,
    actual_focused_ms  INTEGER NOT NULL,   -- frozen here and nowhere else
    completed_task     INTEGER NOT NULL DEFAULT 0,
    lww_hlc_ms         INTEGER NOT NULL DEFAULT 0,
    lww_hlc_logical    INTEGER NOT NULL DEFAULT 0,
    lww_seq            INTEGER NOT NULL DEFAULT 0,
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

-- --- review snapshots ---
-- The op log already holds every number a review reports; what it does NOT
-- hold is the fact that a human sat down and reviewed a given week. That is a
-- new fact, so it is a new op and a new row. Append-only, keyed by its own
-- `rvw_` id, for the reasons ADR-0013 gives for focus sessions.
CREATE TABLE review_snapshots (
    id                 BLOB PRIMARY KEY,   -- the snapshot's own `rvw_` id
    created_at_ms      INTEGER NOT NULL,   -- when the review was completed
    window_start_ms    INTEGER NOT NULL,   -- inclusive start of the window
    window_end_ms      INTEGER NOT NULL,   -- exclusive end
    completed          INTEGER NOT NULL DEFAULT 0,
    deferred           INTEGER NOT NULL DEFAULT 0,
    dropped            INTEGER NOT NULL DEFAULT 0,
    created            INTEGER NOT NULL DEFAULT 0,
    reopened           INTEGER NOT NULL DEFAULT 0,
    body               BLOB NOT NULL,      -- canonical CBOR: per-stream rows,
                                           --   streaks, and the user's note
    lww_hlc_ms         INTEGER NOT NULL DEFAULT 0,
    lww_hlc_logical    INTEGER NOT NULL DEFAULT 0,
    lww_seq            INTEGER NOT NULL DEFAULT 0,
    lww_device         BLOB
);
CREATE INDEX review_snapshots_by_window
    ON review_snapshots (window_start_ms DESC, created_at_ms DESC);

-- --- notes, attachments, people, devices ---
CREATE TABLE notes (
    id              BLOB PRIMARY KEY,
    parent_kind     TEXT NOT NULL,
    parent_id       BLOB NOT NULL,
    body            BLOB NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL,
    deleted         INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX notes_by_parent ON notes (parent_kind, parent_id);

CREATE TABLE attachments (
    id              BLOB PRIMARY KEY,
    parent_kind     TEXT NOT NULL,
    parent_id       BLOB NOT NULL,
    filename        TEXT NOT NULL,
    mime_type       TEXT NOT NULL,
    size_bytes      INTEGER NOT NULL,
    blob_id         BLOB NOT NULL,
    chunk_count     INTEGER NOT NULL,
    content_hash    BLOB NOT NULL,
    deleted         INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE persons (
    id              BLOB PRIMARY KEY,
    display_name    TEXT NOT NULL,
    identity_id     BLOB,
    deleted         INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE devices (
    device_id       BLOB PRIMARY KEY,
    cert_blob       BLOB NOT NULL,
    nickname        TEXT NOT NULL,
    platform        TEXT NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    revoked_at_ms   INTEGER
);

-- --- per-Stream wrapped key store ---
CREATE TABLE stream_keys (
    stream_id       BLOB NOT NULL,
    epoch           INTEGER NOT NULL,
    wrapped         BLOB NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    PRIMARY KEY (stream_id, epoch)
);

-- --- full-text search ---
-- docs/04-storage/local-database.md cites
-- `tokenize = 'unicode61 remove_diacritics 2 porter'`. FTS5 expects chained
-- tokenizers in outer-first order (porter wraps unicode61); args after
-- 'unicode61' configure that base tokenizer.
CREATE VIRTUAL TABLE search_idx USING fts5 (
    kind,
    id UNINDEXED,
    stream_id UNINDEXED,
    title,
    body,
    contexts,
    tokenize = 'porter unicode61 remove_diacritics 2'
);
