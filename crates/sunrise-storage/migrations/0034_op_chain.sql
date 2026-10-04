-- 0034: per-device op chains, the stream digest, and fork evidence
-- (issue #325, ADR-0043).
--
-- What used to happen
-- -------------------
--
-- Each op was signed on its own and linked to nothing, so no replica could
-- tell whether it held the same op set as another. A relay that withheld the
-- last op of a device's sequence left no gap at all, and a device that signed
-- two different ops at one `(stream, seq)` left only whichever arrived first.
--
-- What happens now
-- ----------------
--
-- Every op this vault holds gets its `op_hash` (BLAKE3 of the envelope's full
-- canonical encoding), and every op inside its device's contiguous prefix gets
-- its running chain root `root(device, seq)`. A writer stamps the hash of its
-- own previous op (envelope field 14) and the tips of the other devices it has
-- seen since its last op (field 15). A receiver checks both against what it
-- holds, and records what disagrees as evidence; it never refuses the op.
-- Replicas publish their frontier and its digest as a `StreamDigest` op, and a
-- receiver compares each device's root with its own.
--
-- `op_hash` and `chain_root` are NULL on every row this migration finds. The
-- engine computes them from the stored envelope the first time it folds that
-- device's prefix, so this migration rewrites no row. Both columns are a
-- projection of `envelope`, like every table below that is not evidence.
--
-- Tables
-- ------
--
-- `chain_heads_sent` is the writer's side of field 15: the last seq of each
-- other device's prefix this device has already listed in an op in the stream.
-- An op lists only the devices whose prefix advanced past it.
--
-- `chain_expected` holds an op this replica knows it should hold and does not:
-- its position and hash, named by a later op's field 14 or by another op's
-- field 15 (`named_by` is that op's op_id). A row is deleted when the op at
-- its position arrives, and an arrival whose hash differs is fork evidence.
--
-- `chain_claims` holds a peer's frontier entry this replica could not check
-- yet because its own prefix of that device is shorter: the root the peer
-- claims at `seq`. One row per (stream, device, peer), the highest claim. It
-- is checked and deleted when this replica's prefix reaches `seq`.
--
-- `fork_evidence` holds two signed claims about one position that disagree:
-- this replica holds an op at `(stream_id, device_id, seq)` whose hash is
-- `held_hash`, and `other_envelope`, verified under the device's key, asserts
-- `other_hash` for the same position. `kind` says how: `seq` is a second
-- envelope at the same seq (and `other_envelope` is that envelope), `link` is
-- the next op of the same device naming a different predecessor, `head` is an
-- op of any device naming a different tip. The envelope is kept verbatim, so
-- anyone can re-verify the proof. It is evidence, not a projection: nothing
-- can rebuild it from `ops`.
--
-- `chain_divergence` holds a peer's frontier entry that disagrees with this
-- replica's own root at the same seq: the two hold different ops for that
-- device at or below `seq`. One row per (stream, device, peer), the latest.
ALTER TABLE ops ADD COLUMN op_hash BLOB;
ALTER TABLE ops ADD COLUMN chain_root BLOB;

CREATE TABLE chain_heads_sent (
    stream_id  BLOB NOT NULL,
    device_id  BLOB NOT NULL,
    seq        INTEGER NOT NULL,
    PRIMARY KEY (stream_id, device_id)
);

CREATE TABLE chain_expected (
    stream_id  BLOB NOT NULL,
    device_id  BLOB NOT NULL,
    seq        INTEGER NOT NULL,
    op_hash    BLOB NOT NULL,
    named_by   BLOB NOT NULL,
    PRIMARY KEY (stream_id, device_id, seq, op_hash)
);

CREATE TABLE chain_claims (
    stream_id   BLOB NOT NULL,
    device_id   BLOB NOT NULL,
    claimed_by  BLOB NOT NULL,
    seq         INTEGER NOT NULL,
    root        BLOB NOT NULL,
    PRIMARY KEY (stream_id, device_id, claimed_by)
);

CREATE TABLE fork_evidence (
    stream_id       BLOB NOT NULL,
    device_id       BLOB NOT NULL,
    seq             INTEGER NOT NULL,
    kind            TEXT NOT NULL CHECK (kind IN ('seq', 'link', 'head')),
    held_hash       BLOB NOT NULL,
    other_hash      BLOB NOT NULL,
    other_envelope  BLOB NOT NULL,
    recorded_at_ms  INTEGER NOT NULL,
    PRIMARY KEY (stream_id, device_id, seq, kind, other_hash)
);

CREATE TABLE chain_divergence (
    stream_id       BLOB NOT NULL,
    device_id       BLOB NOT NULL,
    peer_device_id  BLOB NOT NULL,
    seq             INTEGER NOT NULL,
    peer_root       BLOB NOT NULL,
    held_root       BLOB NOT NULL,
    recorded_at_ms  INTEGER NOT NULL,
    PRIMARY KEY (stream_id, device_id, peer_device_id)
);
