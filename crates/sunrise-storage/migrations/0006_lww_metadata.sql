-- Sunrise local storage schema, migration to version 6: LWW metadata.
-- Per the migrations doc: schema changes are always a NEW migration file,
-- never an edit of an existing one.
--
-- Adds entity-level last-writer-wins (LWW) metadata to the materialized
-- `tasks`, `streams`, and `routines` tables so that local and remote writes
-- can compete on the same footing. The winner of a concurrent edit is the one
-- with the greater `(lww_ts_ms, lww_device)` pair, compared lexicographically
-- (ties on `lww_ts_ms` broken by the raw 16-byte device id, memcmp order —
-- see docs/05-sync/conflict-resolution.md). `lww_device` is the 16-byte id of
-- the device whose op last won for the row; NULL only for placeholder rows that
-- no real op has yet stamped (e.g. the lazily-created inbox/meta stream row).
--
-- Column reconciliation with earlier migrations:
--   * `tasks` (0001) had NO created_at_ms / updated_at_ms columns — this
--     migration adds them (they existed only inside the op-log / domain model
--     before). It also adds lww_ts_ms / lww_device.
--   * `streams` (0001) ALREADY has created_at_ms / updated_at_ms — only the two
--     lww_* columns are added here.
--   * `routines` (0004) ALREADY has created_at_ms / updated_at_ms — only the two
--     lww_* columns are added here.
--
-- All new columns are projections (rebuildable from the op log).

ALTER TABLE tasks ADD COLUMN created_at_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN updated_at_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN lww_ts_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN lww_device BLOB;

ALTER TABLE streams ADD COLUMN lww_ts_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE streams ADD COLUMN lww_device BLOB;

ALTER TABLE routines ADD COLUMN lww_ts_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE routines ADD COLUMN lww_device BLOB;
