-- 0052: places, a synced entity (issue #1128, ADR-0051,
-- docs/02-domain/places.md).
--
-- Why
-- ---
--
-- A task could not say where it can be done. `Place` (`plc_`) is now an
-- entity of the vault, written only by `Patch` on the vault-meta stream, one
-- register per field (ADR-0044), and this table is its projection: the
-- merge is its only writer (ADR-0065 §5).
--
-- Coordinates are integers, degrees x 10^7, never floats (ADR-0051 §1).
-- `address_label` is display text only. Like every `Lww` entity the table
-- carries the four `lww_*` stamp columns and an `extra` blob for top-level
-- fields a newer build adds.
--
-- What it never holds is presence. Whether the user is at a place lives in
-- memory on the device that observed it and nowhere else (ADR-0051 §3): no
-- column here, or anywhere in this database, records it.
--
-- Nothing is back-filled. No build before this one wrote a place op it
-- could fold: a build before `DOC_SCHEMA_V` 26 refuses or parks a `Patch`
-- naming the `plc_` prefix, and none is written into a vault until every
-- device advertises `place.entity`. A parked one replays through the full
-- apply path at the first open of this build, which changes `DOC_SCHEMA_V`,
-- and lands here.

CREATE TABLE places (
    id               BLOB PRIMARY KEY,
    name             TEXT NOT NULL,
    lat_e7           INTEGER NOT NULL,
    lon_e7           INTEGER NOT NULL,
    radius_m         INTEGER NOT NULL,
    address_label    TEXT,
    deleted          INTEGER NOT NULL DEFAULT 0,
    created_at_ms    INTEGER NOT NULL DEFAULT 0,
    updated_at_ms    INTEGER NOT NULL DEFAULT 0,
    lww_hlc_ms       INTEGER NOT NULL DEFAULT 0,
    lww_hlc_logical  INTEGER NOT NULL DEFAULT 0,
    lww_seq          INTEGER NOT NULL DEFAULT 0,
    lww_device       BLOB,
    extra            BLOB
);

CREATE INDEX places_live ON places (name) WHERE deleted = 0;
