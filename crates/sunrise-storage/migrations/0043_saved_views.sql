-- 0043: saved views, a synced entity (issue #341,
-- docs/08-features/planning-views.md §Saved views).
--
-- Why
-- ---
--
-- A saved view was a line of a per-device file, `views.toml`, so a view
-- saved on one device did not exist on another, and it named contexts by
-- name, so renaming a context broke it. `SavedView` (`svw_`) is now an
-- entity of the vault, written only by `Patch` on the vault-meta stream, one
-- register per field (ADR-0044), and this table is its projection: the
-- merge is its only writer (ADR-0065 §5).
--
-- `filter` holds the whole `ViewFilter` as canonical CBOR, parts this build
-- does not know included, so the projection is lossless; `view` and `sort`
-- are their spellings, unknown ones verbatim. `sort_order` is the view's
-- place in the saved-views list, NULL for one nobody placed. Like every
-- `Lww` entity it carries the four `lww_*` stamp columns and an `extra` blob
-- for top-level fields a newer build adds.
--
-- Nothing is back-filled. No build before this one wrote a saved-view op it
-- could fold: a build before `DOC_SCHEMA_V` 17 refuses or parks a `Patch`
-- naming the `svw_` prefix, and none is written into a vault until every
-- device advertises `saved_view.entity`. A parked one replays through the
-- full apply path at the first open of this build, which changes
-- `DOC_SCHEMA_V`, and lands here.

CREATE TABLE saved_views (
    id               BLOB PRIMARY KEY,
    name             TEXT NOT NULL,
    view             TEXT NOT NULL,
    filter           BLOB NOT NULL,
    sort             TEXT NOT NULL DEFAULT 'default',
    sort_order       TEXT,
    deleted          INTEGER NOT NULL DEFAULT 0,
    created_at_ms    INTEGER NOT NULL DEFAULT 0,
    updated_at_ms    INTEGER NOT NULL DEFAULT 0,
    lww_hlc_ms       INTEGER NOT NULL DEFAULT 0,
    lww_hlc_logical  INTEGER NOT NULL DEFAULT 0,
    lww_seq          INTEGER NOT NULL DEFAULT 0,
    lww_device       BLOB,
    extra            BLOB
);

CREATE INDEX saved_views_live ON saved_views (sort_order, name) WHERE deleted = 0;
