-- Sunrise local storage schema, version 1.
-- Per spec/04-storage/local-database.md.
-- This file is the single source of truth for STORAGE_V = 1; any change is a
-- new migration file, never an edit of this one.

-- --- pragmas ---
-- (Pragmas are applied programmatically on connection open, not in the
-- migration. See db.rs::Db::open. Listed here for reference:
--   PRAGMA journal_mode   = WAL;
--   PRAGMA synchronous    = NORMAL;
--   PRAGMA foreign_keys   = ON;
--   PRAGMA busy_timeout   = 5000;
--   PRAGMA auto_vacuum    = INCREMENTAL;
-- )

-- --- meta ---
CREATE TABLE schema_meta (
    storage_v       INTEGER NOT NULL,
    applied_at_ms   INTEGER NOT NULL
);
INSERT INTO schema_meta (storage_v, applied_at_ms) VALUES (1, 0);

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

-- --- streams (materialized) ---
CREATE TABLE streams (
    stream_id       BLOB PRIMARY KEY,
    doc_blob        BLOB NOT NULL,
    doc_blob_v      INTEGER NOT NULL,
    head_root       BLOB NOT NULL,
    last_op_seq     INTEGER NOT NULL,
    parent_id       BLOB REFERENCES streams (stream_id),
    archived        INTEGER NOT NULL DEFAULT 0,
    deleted         INTEGER NOT NULL DEFAULT 0,
    created_at_ms   INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL
);

-- --- tasks (materialized) ---
CREATE TABLE tasks (
    id                 BLOB PRIMARY KEY,
    stream_id          BLOB NOT NULL REFERENCES streams (stream_id),
    title              TEXT NOT NULL,
    state              TEXT NOT NULL,
    priority           INTEGER,
    energy             TEXT,
    estimated_min      INTEGER,
    scheduled_at_ms    INTEGER,
    due_at_ms          INTEGER,
    completed_at_ms    INTEGER,
    deferred_count     INTEGER NOT NULL DEFAULT 0,
    routine_id         BLOB,
    routine_occurrence INTEGER,
    archived           INTEGER NOT NULL DEFAULT 0,
    deleted            INTEGER NOT NULL DEFAULT 0,
    body               BLOB,
    extra              BLOB,
    head_root          BLOB
);
CREATE INDEX tasks_by_stream ON tasks (stream_id);
CREATE INDEX tasks_by_due ON tasks (due_at_ms) WHERE due_at_ms IS NOT NULL;

CREATE TABLE task_contexts (
    task_id     BLOB NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    context_id  BLOB NOT NULL,
    PRIMARY KEY (task_id, context_id)
);
CREATE INDEX task_contexts_by_context ON task_contexts (context_id);

-- --- routines, blocks, notes, attachments, persons, devices ---
CREATE TABLE routines (
    id              BLOB PRIMARY KEY,
    stream_id       BLOB NOT NULL REFERENCES streams (stream_id),
    rrule           TEXT NOT NULL,
    timezone        TEXT NOT NULL,
    starts_at_ms    INTEGER NOT NULL,
    ends_at_ms      INTEGER,
    streak_counter  INTEGER NOT NULL DEFAULT 0,
    paused          INTEGER NOT NULL DEFAULT 0,
    archived        INTEGER NOT NULL DEFAULT 0,
    deleted         INTEGER NOT NULL DEFAULT 0,
    extra           BLOB
);

CREATE TABLE blocks (
    id              BLOB PRIMARY KEY,
    stream_id       BLOB NOT NULL REFERENCES streams (stream_id),
    starts_at_ms    INTEGER NOT NULL,
    ends_at_ms      INTEGER NOT NULL,
    title           TEXT,
    deleted         INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX blocks_by_time ON blocks (starts_at_ms);

CREATE TABLE block_tasks (
    block_id        BLOB NOT NULL REFERENCES blocks (id) ON DELETE CASCADE,
    task_id         BLOB NOT NULL,
    PRIMARY KEY (block_id, task_id)
);

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

-- --- conflict-resolution journal ---
CREATE TABLE merge_journal (
    journal_id      BLOB PRIMARY KEY,
    created_at_ms   INTEGER NOT NULL,
    entity_kind     TEXT NOT NULL,
    entity_id       BLOB NOT NULL,
    field           TEXT NOT NULL,
    losing_op_id    BLOB NOT NULL,
    winning_op_id   BLOB NOT NULL,
    summary         TEXT
);
CREATE INDEX merge_journal_by_entity ON merge_journal (entity_kind, entity_id);

-- --- full-text search ---
-- Note: spec/04-storage/local-database.md cites
-- `tokenize = 'unicode61 remove_diacritics 2 porter'`. FTS5 expects chained
-- tokenizers in outer-first order (porter wraps unicode61). Args after
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
