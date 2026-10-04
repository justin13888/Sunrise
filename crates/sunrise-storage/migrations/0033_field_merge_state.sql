-- 0033: per-field merge state (issue #319, ADR-0044).
--
-- What used to happen
-- -------------------
--
-- Every entity row carried one `(hlc, device, seq)` stamp, and an op that
-- beat it replaced the whole row. Two devices that edited different fields of
-- one task at the same time kept only one of the two edits, two concurrent
-- additions to a set kept only one, and two concurrent defers counted once.
--
-- What happens now
-- ----------------
--
-- Each field merges by its own CRDT type, and the state each type needs lives
-- in the tables below. The entity row (`tasks`, `blocks`, ...) is now the read
-- projection of that state: after every op the engine folds the op in here and
-- writes the merged entity back to its row.
--
-- Every table here is a projection of `ops`, like the entity rows: nothing in
-- it is a source of truth, and all of it could be rebuilt by replaying the log.
--
-- An entity that has no `merge_entities` row has no field state yet. That is
-- every entity a vault held when it took this migration. The engine seeds one
-- from the entity's row the first time an op touches it, reading the row as a
-- legacy full-state op at the row's stamp (ADR-0044 §7), so this migration
-- rewrites no row and needs no backfill.
--
-- Tables
-- ------
--
-- `merge_entities` holds one row per entity with field state. `kind` is the
-- registry tag. `created` is 1 once a create, or any legacy full-state op, has
-- been applied: only then is the entity projected (ADR-0044 §4). `create_*`
-- is the least stamp among those ops, which `created_at` reads from when no
-- legacy op wrote it. `legacy_*` is the greatest stamp among the legacy
-- full-state ops applied, which every legacy rule is evaluated against.
-- `patch_ms` is the greatest `hlc.physical_ms` among the `Patch` ops applied,
-- which `updated_at` reads from (ADR-0044 §10). `head_*` is the greatest stamp
-- of any op applied, written to the entity row's LWW columns. `row_*` is the
-- row stamp the merge last wrote or saw: a row whose stamp differs was written
-- by a local full-state command, and the merge folds the row in as that
-- command's op before it folds the next one.
--
-- A stamp is `(hlc_ms, hlc_logical, device, seq, stream)`, compared in that
-- order. `stream` completes it: `seq` counts per `(stream, device)`, so it is
-- what tells apart two ops of one device that are otherwise equal.
--
-- `merge_registers` holds one last-writer-wins register per field, including
-- the fields this build does not know, nested fields named `parent.child`, and
-- the legacy base of a counter. `value` is canonical CBOR; NULL means the
-- writer left the field out, so it reads as the field's default. `origin` is
-- `user` or `generated` (ADR-0044 §3).
--
-- `merge_map_entries` holds the per-key registers of a map field. A CBOR null
-- value is a tombstoned key.
--
-- `merge_orset_adds` and `merge_orset_removes` are an add-wins
-- observed-remove set per field. An add is tagged with the op-ref of the op
-- that made it, `(tag_stream, tag_device, tag_seq)`; a remove names the
-- element and one tag it had observed.
--
-- `merge_counter_deltas` holds each applied `inc`, once per op.
CREATE TABLE merge_entities (
    entity_id           BLOB PRIMARY KEY,
    kind                TEXT NOT NULL,
    created             INTEGER NOT NULL DEFAULT 0,
    create_hlc_ms       INTEGER,
    create_hlc_logical  INTEGER,
    create_device       BLOB,
    create_seq          INTEGER,
    create_stream       BLOB,
    legacy_hlc_ms       INTEGER,
    legacy_hlc_logical  INTEGER,
    legacy_device       BLOB,
    legacy_seq          INTEGER,
    legacy_stream       BLOB,
    patch_ms            INTEGER,
    head_hlc_ms         INTEGER NOT NULL,
    head_hlc_logical    INTEGER NOT NULL,
    head_device         BLOB NOT NULL,
    head_seq            INTEGER NOT NULL,
    head_stream         BLOB NOT NULL,
    row_hlc_ms          INTEGER,
    row_hlc_logical     INTEGER,
    row_device          BLOB,
    row_seq             INTEGER
);

CREATE TABLE merge_registers (
    entity_id    BLOB NOT NULL,
    field        TEXT NOT NULL,
    value        BLOB,
    hlc_ms       INTEGER NOT NULL,
    hlc_logical  INTEGER NOT NULL,
    device       BLOB NOT NULL,
    seq          INTEGER NOT NULL,
    stream       BLOB NOT NULL,
    origin       TEXT NOT NULL,
    PRIMARY KEY (entity_id, field)
);

CREATE TABLE merge_map_entries (
    entity_id    BLOB NOT NULL,
    field        TEXT NOT NULL,
    map_key      TEXT NOT NULL,
    value        BLOB NOT NULL,
    hlc_ms       INTEGER NOT NULL,
    hlc_logical  INTEGER NOT NULL,
    device       BLOB NOT NULL,
    seq          INTEGER NOT NULL,
    stream       BLOB NOT NULL,
    origin       TEXT NOT NULL,
    PRIMARY KEY (entity_id, field, map_key)
);

CREATE TABLE merge_orset_adds (
    entity_id    BLOB NOT NULL,
    field        TEXT NOT NULL,
    element      BLOB NOT NULL,
    tag_stream   BLOB NOT NULL,
    tag_device   BLOB NOT NULL,
    tag_seq      INTEGER NOT NULL,
    hlc_ms       INTEGER NOT NULL,
    hlc_logical  INTEGER NOT NULL,
    PRIMARY KEY (entity_id, field, element, tag_stream, tag_device, tag_seq)
);
-- A restored context re-projects the tasks whose `contexts` set names it.
CREATE INDEX merge_orset_adds_by_element ON merge_orset_adds (field, element);

CREATE TABLE merge_orset_removes (
    entity_id    BLOB NOT NULL,
    field        TEXT NOT NULL,
    element      BLOB NOT NULL,
    tag_stream   BLOB NOT NULL,
    tag_device   BLOB NOT NULL,
    tag_seq      INTEGER NOT NULL,
    PRIMARY KEY (entity_id, field, element, tag_stream, tag_device, tag_seq)
);

CREATE TABLE merge_counter_deltas (
    entity_id    BLOB NOT NULL,
    field        TEXT NOT NULL,
    op_stream    BLOB NOT NULL,
    op_device    BLOB NOT NULL,
    op_seq       INTEGER NOT NULL,
    hlc_ms       INTEGER NOT NULL,
    hlc_logical  INTEGER NOT NULL,
    delta        INTEGER NOT NULL,
    PRIMARY KEY (entity_id, field, op_stream, op_device, op_seq)
);
