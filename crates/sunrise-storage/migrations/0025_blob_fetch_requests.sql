-- 0025: the queue for an attachment this device has been *asked* to fetch.
--
-- `docs/02-domain/attachments.md` §Lazy fetch gives an attachment two routes
-- to a device that holds only its metadata. The first was built by 0024 and
-- `blob_sync`: under the 10 MiB auto-fetch threshold, the sync driver goes and
-- gets it without being asked. The second was not built at all (issue #227):
-- "larger attachments show an inline placeholder with file name, size, and a
-- 'Download' button", and the button had nothing behind it. An attachment over
-- the threshold was therefore not slow to reach a second device -- it was
-- unreachable there, permanently, with the ciphertext sitting on the relay and
-- the metadata sitting in the row this table now references.
--
-- One row per attachment the user has asked for. Inserted by
-- `Core::fetch_attachment`, deleted when the chunks land.
--
-- Why a durable row rather than an in-memory request
-- --------------------------------------------------
--
-- The same reason `blob_uploads` above it is durable, read from the other
-- direction. The transport belongs to the sync driver task, so the request has
-- to cross a task boundary to be acted on at all; and a 100 MB download over a
-- link that drops is a request that has to survive the drop, because the
-- alternative is a user who pressed Download, watched it fail, and has to find
-- the attachment again. It also survives the process: a request made on a
-- train is still a request when the app reopens.
--
-- `state`
-- -------
--
-- Two values, and the second is the document's word.
--
--   'requested' -- the user asked and this device has not finished. The driver
--                  fetches these, ignoring the auto-fetch threshold entirely:
--                  the threshold is a policy about what to fetch *unasked*,
--                  and there is nothing unasked about this row.
--   'partial'   -- "The 'Cancel' button during transfer aborts and marks the
--                  attachment `partial: true` in cache." Also where an attempt
--                  ceiling lands, which is the same fact from the other side:
--                  a transfer was started and did not finish.
--
-- There is no 'done'. A finished fetch has the chunks in the blob store, which
-- is a stronger statement than a row claiming so, and `BlobStore::has_all` is
-- already the question every reader asks. A third state would be a second
-- answer that can disagree with the first.
--
-- Why `partial` is here and not on `attachments`
-- ----------------------------------------------
--
-- Because it is a fact about *this device's cache*, which is what the document
-- calls it, and `attachments` is a replicated CRDT row. `sunrise_domain`'s
-- attachment is write-once by construction -- "the metadata is written once and
-- thereafter only tombstoned... there is no `AttachmentPatch` and no update op"
-- -- so marking it would have to be either an un-synced column bolted onto a
-- synced table, or an op telling every device in the account that one of them
-- cancelled a download. Neither is true to what happened.
--
-- `attempts` / `last_attempt_ms` carry 0020's and 0024's meaning: a relay that
-- cannot serve the blob becomes visible rather than retried forever.
CREATE TABLE blob_fetches (
    attachment_id   BLOB PRIMARY KEY,
    blob_id         BLOB NOT NULL,
    state           TEXT NOT NULL,
    requested_at_ms INTEGER NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_attempt_ms INTEGER
) WITHOUT ROWID;

-- The driver takes the oldest requests first: a user who asked twice is
-- waiting on the first answer.
CREATE INDEX blob_fetches_by_age ON blob_fetches (state, requested_at_ms);
