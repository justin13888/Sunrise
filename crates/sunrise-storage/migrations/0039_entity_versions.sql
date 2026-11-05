-- 0039: the version DAG of every mergeable entity, the losers a register
-- keeps, the members of a group that wait for the rest, and the record of
-- the last projection rebuild (ADR-0065 §1, §2, §4, §6, §7; issue #447).
--
-- Why
-- ---
--
-- ADR-0044 ordered two writes of one field by their stamps alone, so it could
-- not tell "B overwrote A's title having seen it" from "B wrote a title
-- without knowing A had". ADR-0065 makes every op on a mergeable entity a
-- *version*, with the entity's heads its writer had applied as its
-- `parents`, and decides concurrency from that DAG. These tables are the
-- local index of it. Every one of them is a projection of `ops`, like the
-- `merge_*` tables of migration 0033: nothing here is a source of truth, and
-- `Engine::rebuild_projection` derives all of it again from the log.
--
-- The tables start empty, and the engine fills them by rebuilding the
-- projection once, in the first `Core::open` after this migration
-- (`projection_rebuild` below). That rebuild also replaces what migration
-- 0033 left to the first op that touched an entity: an entity row written
-- with no field state behind it, by a vault older than 0033 or by a build
-- before ADR-0065 PR 3, is now folded from its ops rather than read back from
-- the row. So `merge_entities.row_*` is no longer read or written. The four
-- columns stay, NULL on every row the merge writes from now on, because
-- dropping a column is a cost with no benefit (`migrations.md` §Migration
-- rigor rule 5).
--
-- Tables
-- ------
--
-- `entity_versions` holds one row per version: an applied entity op of an
-- entity that merges field by field. Its id is the op's op-ref
-- `(stream, device, seq)`, and `hlc_*` with it is the op's stamp. `kind` is
-- `legacy` for a full-state op and `patch` for a `Patch`; `origin` is `user`
-- or `generated` (ADR-0044 §3); `is_create` is 1 for a `*Create` op and a
-- `Patch` with `create`. `parents` is the op's `parents` as canonical CBOR,
-- `[* [stream, device, seq]]`, and NULL when the key is absent, which is not
-- the same as `[]`. `group_id` and `group_size` are its group, if any.
-- `fields` is the sorted field names it wrote, as a canonical CBOR array; a
-- legacy op writes every field. `is_head` is 1 when no other version of the
-- entity descends from it.
--
-- `entity_frontier` holds each version's causal frontier `F(v)`: for every
-- device, the greatest stamp of that device's versions of the entity that
-- `v` reaches through its parents and through the same device's previous
-- version, `v` included (ADR-0065 §2 and its clarifications D1–D3). A
-- version `a` precedes `b` exactly when `F(b)` at `a`'s device is at least
-- `a`'s stamp.
--
-- `register_losers` holds, per register, the concurrent writes that lost to
-- the winner `merge_registers` holds (ADR-0065 §4). `legacy` is 1 for a
-- write a full-state op made. Nothing writes it until the register fold
-- reads conflict classes.
--
-- `pending_groups` holds the members of a group (ADR-0065 §6) that have
-- arrived, by group and op, with each member's stamp; `applied` is 1 once
-- the group has folded. Nothing writes it until groups are applied whole.
--
-- `projection_rebuild` is the one row recording the last full rebuild of the
-- projection: the version of the fold it ran (`FOLD_V` in the engine's merge
-- module) and when it finished. An open that finds no row, or a different
-- `fold_v`, rebuilds before anything reads the projection. It is local
-- bookkeeping, not a projection, and a rebuild does not clear it.

CREATE TABLE entity_versions (
    entity_id    BLOB NOT NULL,
    stream       BLOB NOT NULL,
    device       BLOB NOT NULL,
    seq          INTEGER NOT NULL,
    hlc_ms       INTEGER NOT NULL,
    hlc_logical  INTEGER NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('legacy', 'patch')),
    origin       TEXT NOT NULL,
    is_create    INTEGER NOT NULL,
    parents      BLOB,
    group_id     BLOB,
    group_size   INTEGER,
    fields       BLOB NOT NULL,
    is_head      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (entity_id, stream, device, seq)
) WITHOUT ROWID;
-- An entity's heads: the parents a writer names.
CREATE INDEX entity_versions_heads ON entity_versions (entity_id) WHERE is_head = 1;
-- One device's versions of an entity in stamp order: its previous version.
CREATE INDEX entity_versions_by_device
    ON entity_versions (entity_id, device, hlc_ms, hlc_logical, seq, stream);
-- An entity's versions in stamp order: those stamped after a late arrival.
CREATE INDEX entity_versions_by_stamp
    ON entity_versions (entity_id, hlc_ms, hlc_logical, device, seq, stream);

CREATE TABLE entity_frontier (
    entity_id    BLOB NOT NULL,
    v_stream     BLOB NOT NULL,
    v_device     BLOB NOT NULL,
    v_seq        INTEGER NOT NULL,
    device       BLOB NOT NULL,
    hlc_ms       INTEGER NOT NULL,
    hlc_logical  INTEGER NOT NULL,
    seq          INTEGER NOT NULL,
    stream       BLOB NOT NULL,
    PRIMARY KEY (entity_id, v_stream, v_device, v_seq, device)
) WITHOUT ROWID;

CREATE TABLE register_losers (
    entity_id    BLOB NOT NULL,
    field        TEXT NOT NULL,
    stream       BLOB NOT NULL,
    device       BLOB NOT NULL,
    seq          INTEGER NOT NULL,
    hlc_ms       INTEGER NOT NULL,
    hlc_logical  INTEGER NOT NULL,
    value        BLOB,
    origin       TEXT NOT NULL,
    legacy       INTEGER NOT NULL,
    PRIMARY KEY (entity_id, field, stream, device, seq)
) WITHOUT ROWID;

CREATE TABLE pending_groups (
    group_id        BLOB NOT NULL,
    group_size      INTEGER NOT NULL,
    op_id           BLOB NOT NULL,
    hlc_ms          INTEGER NOT NULL,
    hlc_logical     INTEGER NOT NULL,
    device          BLOB NOT NULL,
    seq             INTEGER NOT NULL,
    stream          BLOB NOT NULL,
    applied         INTEGER NOT NULL DEFAULT 0,
    received_at_ms  INTEGER NOT NULL,
    PRIMARY KEY (group_id, group_size, op_id)
) WITHOUT ROWID;
CREATE INDEX pending_groups_by_op ON pending_groups (op_id);

CREATE TABLE projection_rebuild (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    fold_v      INTEGER NOT NULL,
    done_at_ms  INTEGER NOT NULL
);
