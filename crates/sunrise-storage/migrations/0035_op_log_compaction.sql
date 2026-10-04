-- 0035: client op-log compaction below a snapshot floor (issue #330,
-- ADR-0059, docs/04-storage/compaction.md).
--
-- What used to happen
-- -------------------
--
-- Every op a device received stayed in `ops` forever. The relay trimmed its
-- own copy at 30 days or 256 MiB per channel, but no client ever trimmed its
-- own, so storage, cold-open time and rebuild time all grew with the vault's
-- age. A device offline for longer than the relay keeps ops had nothing to
-- catch up from.
--
-- What happens now
-- ----------------
--
-- Each `(stream, device)` prefix can have a floor. Every op of that device in
-- that stream at or below the floor is covered: its effect is in the per-field
-- merge state (migration 0033) or it is still held as a row, and the floor's
-- chain root (migration 0034) stands for the whole prefix below it. The op-log
-- writer and receiver read the floor wherever they used to read the op row at
-- that position, so the contiguous prefix, the chain root and the stream
-- digest all start from it.
--
-- A floor rises in two ways. A local compaction raises it to a frontier every
-- known device has acknowledged in its stream digest, and deletes the entity
-- ops below it whose effect is in the merge state. Applying a snapshot raises
-- it to the snapshot's frontier, after the snapshot's state is joined in.
--
-- Tables
-- ------
--
-- `compaction_floor` holds one floor per `(stream_id, device_id)`: `seq`, the
-- `op_hash` of the op at `seq`, and `root`, the chain root `root(device, seq)`.
-- A floor never falls.
--
-- `peer_frontiers` holds the last frontier each peer published in a stream, one
-- row per `(stream, peer, device)`: the peer holds every op of `device` through
-- `seq`. It is what "every known device has acknowledged" is read from.
-- `recorded_at_ms` is the digest op's own stamp, so it also says when the peer
-- was last heard from. It is not a projection of `ops`: the digest ops it was
-- read from are themselves compacted.
--
-- `stream_snapshots` holds the latest snapshot record per stream that this
-- replica generated as the stream's compactor, or applied. `record` is the
-- sealed and signed record, magic kind 4, verbatim; `digest` is the stream
-- digest of its frontier.
CREATE TABLE compaction_floor (
    stream_id  BLOB NOT NULL,
    device_id  BLOB NOT NULL,
    seq        INTEGER NOT NULL,
    op_hash    BLOB NOT NULL,
    root       BLOB NOT NULL,
    PRIMARY KEY (stream_id, device_id)
);

CREATE TABLE peer_frontiers (
    stream_id       BLOB NOT NULL,
    peer_device_id  BLOB NOT NULL,
    device_id       BLOB NOT NULL,
    seq             INTEGER NOT NULL,
    recorded_at_ms  INTEGER NOT NULL,
    PRIMARY KEY (stream_id, peer_device_id, device_id)
);

CREATE TABLE stream_snapshots (
    stream_id        BLOB PRIMARY KEY,
    generated_by     BLOB NOT NULL,
    generated_at_ms  INTEGER NOT NULL,
    digest           BLOB NOT NULL,
    record           BLOB NOT NULL
);
