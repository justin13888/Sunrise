-- 0031: park a verified op this build cannot read, instead of dropping it as
-- corruption (issue #320, ADR-0045 §4).
--
-- What used to happen
-- -------------------
--
-- An envelope whose signature verified and whose payload opened under a key
-- this vault holds, but whose inner op kind this build does not know, was
-- reported as `RemoteOpInvalid`, classed as link damage by the sync driver,
-- and written nowhere. It did not reach `ops`, so it did not advance the sync
-- cursor either; the relay kept re-sending it until its bounded log evicted
-- the frame, and then it was gone. A laptop that upgraded before its phone
-- lost every op of the new kind that the phone received, and upgrading the
-- phone afterwards brought none of them back.
--
-- What happens now
-- ----------------
--
-- The op goes into `ops` like any other delivery, with `applied_at` NULL and
-- `inner_kind` / `target_kind` both `'unknown'`, and it gets a row here. The
-- `ops` row is what makes it count as received: the contiguous
-- `(stream, device, seq)` prefix in `upsert_sync_cursor` reads `ops`, so the
-- cursor moves past a parked op exactly as it moves past an applied one, and
-- the relay stops re-sending it. This table is only the marker and the replay
-- index.
--
-- When a build opens the vault whose `DOC_SCHEMA_V` differs from the one in
-- `parked_under_doc_schema_v`, it retries every such row through the full apply
-- path in `(hlc, device_id, seq)` order. A retry that applies deletes the row
-- and fills in the `ops` row's kind columns and `applied_at`, in the same
-- transaction as the materialization. A retry that still cannot apply stays
-- here, re-stamped with the build that tried, so the next open does not try
-- again until the build changes.
--
-- Why there is no TTL and no cap
-- ------------------------------
--
-- `deferred_ops` (0017) has both, because what it holds has not been opened:
-- it is ciphertext at an attacker-chosen `(stream, epoch)`, and dropping it is
-- recoverable because it never advanced the cursor, so the relay re-sends it.
-- Neither is true here. A parked op verified under a member's device key and
-- opened under a Stream key this vault holds, which is exactly the standing
-- that writes an ordinary op into `ops` forever; and it has advanced the
-- cursor, so dropping it is permanent loss. ADR-0045's invariant is that a
-- verified op is never dropped.
--
-- Columns
-- -------
--
-- `reason` is why the op is parked. This build writes one reason,
-- `unknown_kind`; ADR-0045 §4 lists the others, which arrive with #322 and
-- #323, so it is not constrained to a closed set here.
-- `kind` is the unknown variant name, for diagnostics only. Nothing branches
-- on it.
-- `hlc_logical` completes the replay order: `ops.ts_ms` already holds the
-- envelope's `hlc.physical_ms`, and `ops` has no logical column.
CREATE TABLE parked_ops (
    op_id                     BLOB PRIMARY KEY,
    reason                    TEXT NOT NULL,
    kind                      TEXT NOT NULL,
    hlc_logical               INTEGER NOT NULL,
    parked_under_doc_schema_v INTEGER NOT NULL,
    parked_at_ms              INTEGER NOT NULL
);
