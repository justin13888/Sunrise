-- 0022: the two facts an attachment needs before its bytes can leave the device.
--
-- Until this migration an attachment was complete on the machine that made it
-- and permanently incomplete everywhere else (issue #176): `Core::attach_file`
-- sealed the chunks into the local blob store, the metadata op synced, and
-- nothing ever called the relay's `init` -> `PUT` -> `finalize`. Two things
-- were missing, and they are the two things here.

-- 1. The name the relay knows a blob by.
--
-- `POST /blobs/finalize` content-addresses a committed blob at the first
-- sixteen bytes of BLAKE3 over its *ciphertext*, and `GET /blobs/{blob_id}`
-- answers to that and nothing else. A replica holding the metadata cannot
-- derive it -- it would have to hash the bytes it is asking for -- and
-- `content_hash` is over the plaintext, so it is not that value either. The
-- sealing device is the one place the ciphertext exists in full, so it records
-- the hash there and the op carries it.
--
-- `DEFAULT X''` rather than a backfill: nothing can compute this for a row
-- written before the column existed. A zero-width value reads back as the
-- all-zero hash, which `Attachment::is_fetchable` treats as "not on the relay"
-- -- true of every one of those rows, since no client had ever uploaded.
ALTER TABLE attachments ADD COLUMN ciphertext_hash BLOB NOT NULL DEFAULT X'';

-- 2. The queue of "these sealed chunks have not reached the relay yet".
--
-- The same shape as `relay_revocation_intents` (0020) and for the same reason:
-- attaching a file must work offline, so the upload cannot be part of the
-- command, and a durable row is what survives the close that happens between
-- attaching on a plane and landing.
--
-- One row per blob, inserted by `Core::attach_file` after the chunks are on
-- disk, deleted when the relay has committed the blob.
--
-- `upload_id` is the whole idempotency story. It is `NULL` until `blobs/init`
-- answers and is then **kept**, so every retry re-uses it. The relay keys its
-- pending area by upload id and by nothing else, so re-`PUT`ting under the same
-- id overwrites in place: a retry cannot leave a second pending directory
-- behind, which is exactly what a fresh `init` per attempt would do -- one
-- orphaned directory per attempt, each holding a full copy of the attachment,
-- and nothing sweeping them.
--
-- `attempts` and `last_attempt_ms` carry the same meaning as 0020's: a relay
-- that is refusing the upload is visible rather than retried forever.
CREATE TABLE blob_uploads (
    blob_id         BLOB PRIMARY KEY,
    stream_id       BLOB NOT NULL,
    chunk_count     INTEGER NOT NULL,
    size_bytes      INTEGER NOT NULL,
    ciphertext_hash BLOB NOT NULL,
    upload_id       TEXT,
    attempts        INTEGER NOT NULL DEFAULT 0,
    created_at_ms   INTEGER NOT NULL,
    last_attempt_ms INTEGER
) WITHOUT ROWID;

CREATE INDEX blob_uploads_by_age ON blob_uploads (created_at_ms);
