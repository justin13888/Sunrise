-- 0054: context shifts, a synced entity, and each routine's shift policy
-- (issue #1244, ADR-0073).
--
-- Why
-- ---
--
-- A user away from their usual life (a trip, a sabbatical) had nothing to
-- say so with. `Shift` (`shf_`) is now an entity of the vault, written only
-- by `Patch` on the vault-meta stream, one register per field (ADR-0044),
-- and `shifts` is its projection: the merge is its only writer (ADR-0065
-- §5). Like every `Lww` entity the table carries the four `lww_*` stamp
-- columns and an `extra` blob for top-level fields a newer build adds.
--
-- `primary_streams` and `primary_contexts` are each one register holding a
-- set of refs, stored as canonical CBOR, as `routines.skipped_keys` is.
--
-- Each routine gains `shift_policy`, what it does while a shift is in force
-- (ADR-0073 §3). Absent is `pause`, so every existing row is `pause` and
-- nothing is rewritten. The column holds the spelling, an unknown one from a
-- newer build included, so it is written back verbatim.
--
-- Nothing is back-filled. No build before this one wrote a shift op it
-- could fold: a build before `DOC_SCHEMA_V` 31 parks a `Patch` naming the
-- `shf_` prefix, and none is written into a vault until every device
-- advertises `shift.entity`. A parked one replays through the full apply
-- path at the first open of this build, which changes `DOC_SCHEMA_V`, and
-- lands here. No routine op carries a policy other than `pause` until every
-- device advertises `routine.shift_policy`, so no existing row holds one.

CREATE TABLE shifts (
    id                BLOB PRIMARY KEY,
    name              TEXT NOT NULL,
    starts_at_ms      INTEGER NOT NULL,
    ends_at_ms        INTEGER,
    zone              TEXT,
    place             BLOB,
    primary_streams   BLOB,
    primary_contexts  BLOB,
    deleted           INTEGER NOT NULL DEFAULT 0,
    created_at_ms     INTEGER NOT NULL DEFAULT 0,
    updated_at_ms     INTEGER NOT NULL DEFAULT 0,
    lww_hlc_ms        INTEGER NOT NULL DEFAULT 0,
    lww_hlc_logical   INTEGER NOT NULL DEFAULT 0,
    lww_seq           INTEGER NOT NULL DEFAULT 0,
    lww_device        BLOB,
    extra             BLOB
);

CREATE INDEX shifts_live ON shifts (starts_at_ms) WHERE deleted = 0;

ALTER TABLE routines ADD COLUMN shift_policy TEXT NOT NULL DEFAULT 'pause';
