-- 0016_stream_description_and_default_context.sql — give two declared Stream
-- fields somewhere to live.
--
-- `docs/02-domain/streams.md` has declared both in its CDDL since the
-- beginning, `Stream` carries both in Rust, and `StreamDraft` accepts
-- `description`. Neither had a column.
--
-- `description` was the worse of the two, because it looked like it worked.
-- `create_stream` copied it onto the entity and `update_stream` applied the
-- patch, so the value rode out in the op and every peer received it — and
-- then `read_stream` hardcoded `description: None`, so no replica, including
-- the authoring device, could ever materialize it. The next `UpdateStream`
-- read `None` and re-emitted `description: null`, which under entity-level LWW
-- (ADR-0014) overwrote the value on every replica that still had it. A field
-- the UniFFI seam exposes as `StreamItem.description` and `StreamEdit
-- .set_description`: a client could set it, watch it vanish, and be right.
--
-- `default_context` never worked at all — no column, no patch field, hardcoded
-- `None` on both create and read — so it was unreachable rather than lossy.
--
-- Both are BLOB. `description` is a `NoteBody`, which is a newtype over the
-- block-grammar bytes and is stored exactly as `tasks.body` is; a 16-byte
-- entity id is what `default_context` holds, matching `parent_id` above it.
--
-- Additive and backfill-free: NULL is the correct reading for every existing
-- row, because no row has ever held either value.

ALTER TABLE streams ADD COLUMN description     BLOB;
ALTER TABLE streams ADD COLUMN default_context BLOB;
