-- 0038: attachment thumbnails and dimensions, and this device's blob cache
-- index (issue #346, ADR-0053, docs/02-domain/attachments.md).
--
-- Why
-- ---
--
-- `attachments` gains the eight optional `attachment.thumbnail` fields. Each
-- is NULL on every row written before them, which is what "this attachment
-- has no thumbnail" means, so nothing is backfilled. A row a build without
-- these columns wrote from an op that carried them holds them in `extra`;
-- the engine reads them out of there when the column is NULL, so they are not
-- lost by the upgrade.
--
-- `blob_cache` is this device's index of the sealed blobs in its local blob
-- store: how large each is, when it was last opened, whether it is a
-- thumbnail, and whether it has been evicted. The Rust core's LRU reads it to
-- decide what to evict and what the cache holds. It is never an op and never
-- synced. It starts empty; the core indexes the blobs already on disk the
-- first time it enforces the limit.
--
-- `blob_cache_backfill` records that this first pass has run, in its one row.
-- The pass probes the blob store for every attachment, so it runs once per
-- vault: every blob that reaches the store after it is indexed by the path
-- that wrote it, and a scan repeated at every launch would probe each
-- never-fetched remote original again, forever.

ALTER TABLE attachments ADD COLUMN width INTEGER;
ALTER TABLE attachments ADD COLUMN height INTEGER;
ALTER TABLE attachments ADD COLUMN thumbnail_blob_id BLOB;
ALTER TABLE attachments ADD COLUMN thumbnail_blob_key BLOB;
ALTER TABLE attachments ADD COLUMN thumbnail_mime TEXT;
ALTER TABLE attachments ADD COLUMN thumbnail_size_bytes INTEGER;
ALTER TABLE attachments ADD COLUMN thumbnail_content_hash BLOB;
ALTER TABLE attachments ADD COLUMN thumbnail_ciphertext_hash BLOB;

CREATE TABLE blob_cache (
    blob_id         BLOB PRIMARY KEY,
    sealed_bytes    INTEGER NOT NULL,
    last_access_ms  INTEGER NOT NULL,
    is_thumbnail    INTEGER NOT NULL DEFAULT 0,
    evicted_at_ms   INTEGER
);

CREATE INDEX blob_cache_by_access ON blob_cache (last_access_ms, blob_id);

CREATE TABLE blob_cache_backfill (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    done_at_ms  INTEGER NOT NULL
);
