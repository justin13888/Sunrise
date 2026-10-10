-- 0037: the vault's Preferences entity and this device's preference overlay
-- (issue #337, ADR-0050, docs/02-domain/preferences.md).
--
-- Why
-- ---
--
-- Every setting lived in a client's own store (UserDefaults on Apple), so
-- nothing a user configured followed them to another device, and the core
-- could not read a value it needed (week start, staleness, lead times).
--
-- `preferences` is the projection of the one synced `Preferences` entity per
-- vault, id `prf_` + the zero body. It is written only by the per-field merge
-- (ADR-0044): every key of the entity's `values` field is its own register in
-- `merge_map_entries`, and this row is what those registers read as.
-- `values_cbor` holds the merged map as canonical CBOR, keys this build does
-- not know included, so the projection is lossless. Like every `Lww` entity it carries
-- the four `lww_*` stamp columns and an `extra` blob for top-level fields a
-- newer build adds.
--
-- `device_preferences` is this device's overlay: the values of `device`-scoped
-- keys, and this device's override of `vault_overridable` ones. It is never an
-- op and never synced; `value` is canonical CBOR. A key this build does not
-- know is left in place, not deleted.

CREATE TABLE preferences (
    id               BLOB PRIMARY KEY,
    values_cbor      BLOB NOT NULL,
    created_at_ms    INTEGER NOT NULL DEFAULT 0,
    updated_at_ms    INTEGER NOT NULL DEFAULT 0,
    lww_hlc_ms       INTEGER NOT NULL DEFAULT 0,
    lww_hlc_logical  INTEGER NOT NULL DEFAULT 0,
    lww_seq          INTEGER NOT NULL DEFAULT 0,
    lww_device       BLOB,
    extra            BLOB
);

CREATE TABLE device_preferences (
    key    TEXT PRIMARY KEY,
    value  BLOB NOT NULL
);
