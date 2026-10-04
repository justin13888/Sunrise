//! Per-device op chains, causal heads, fork evidence and the stream digest
//! (ADR-0043, issue #325).
//!
//! # What a writer stamps
//!
//! [`writer_links`] fills envelope field 14 with the [`op_hash`] of this
//! device's own op at `seq - 1`, read from its op log, and field 15 with the
//! tip of every other device's contiguous prefix in the stream that advanced
//! since this device last listed it (`chain_heads_sent`). A device that cannot
//! find its own predecessor writes no field 14: a legacy link asserts nothing,
//! where a guessed one would assert something false (ADR-0043 §1).
//!
//! # What a receiver checks
//!
//! [`check_links`] runs once per op that enters the log. It never refuses the
//! op: ADR-0034 established that no replica refuses one, and ADR-0044 makes
//! every apply order-independent, so a link that does not hold is evidence to
//! keep, not a reason to diverge (ADR-0043 §3). The four cases:
//!
//! | Case | Outcome |
//! |---|---|
//! | Linked: `seq - 1` is held and hashes to field 14 | nothing to record |
//! | Gap: `seq - 1` is not held | `chain_expected` records the hash it must have |
//! | Fork: `seq - 1` is held and hashes to something else | `fork_evidence`, kind `link` |
//! | Legacy: no field 14 | nothing asserted |
//!
//! Field 15 is checked the same way, position by position, with kind `head`.
//! An op that arrives where a `chain_expected` row waits is checked against
//! it. [`check_duplicate`] covers the last way two signed claims can meet: a
//! second envelope at a `(stream, device, seq)` the log already holds, kind
//! `seq`. Only the first stays in `ops` and is materialized; the second is
//! kept verbatim in `fork_evidence`, where anyone can re-verify it.
//!
//! # The chain root and the stream digest
//!
//! [`fold_chain`] keeps `ops.chain_root = root(device, seq)` for every op
//! inside its device's contiguous prefix, one step per op, and checks any
//! peer claim the prefix has just reached. It runs from
//! [`super::oplog::upsert_sync_cursor`], so the root moves exactly when the
//! cursor does. [`frontier`] reads the prefix ends and their roots, and
//! [`reconcile`] compares a peer's [`StreamDigestPayload`] with them: an entry
//! this replica can check and that disagrees is a `chain_divergence`, and one
//! it cannot check yet, because its own prefix is shorter, is a
//! `chain_claims` row, which is also how this replica learns it is missing
//! ops a relay never sent.
//!
//! Every hash here is computed from envelope bytes this replica holds, so the
//! chain root covers ops written before chaining existed: field 14 makes the
//! writer attest the order, and the digest makes replicas agree on it.

use rusqlite::{params, OptionalExtension, Transaction};
use sunrise_crypto::{
    chain_root_init, chain_root_step, decode_envelope, op_hash, stream_digest, ChainHead,
    ChainLinks, FrontierEntry, OpEnvelope, MAX_CHAIN_HEADS,
};

use super::ids::hex_short;
use super::{Engine, EngineError};
use crate::inner_op::{encode_inner_op, FrontierWire, InnerOp, StreamDigestPayload};
use sunrise_domain::Unknowns;
use sunrise_storage::Db;

/// How one position's two claims disagree. Stored in `fork_evidence.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForkKind {
    /// A second envelope at a `(stream, device, seq)` the log already holds.
    Seq,
    /// The next op of the same device names a different predecessor.
    Link,
    /// An op lists, in field 15, a different op at a position this replica
    /// holds.
    Head,
}

impl ForkKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Seq => "seq",
            Self::Link => "link",
            Self::Head => "head",
        }
    }
}

/// What the replica holds about chains, for an integrity indicator and for the
/// sync driver.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChainIntegrity {
    /// Rows of `fork_evidence`: positions where two signed claims disagree.
    pub forks: u64,
    /// Rows of `chain_divergence`: peer frontier entries that disagree with
    /// this replica's own root at the same seq.
    pub divergences: u64,
    /// Ops this replica knows it should hold and does not: rows of
    /// `chain_expected` and `chain_claims`.
    pub wanted: u64,
}

fn to32(v: &[u8]) -> Option<[u8; 32]> {
    v.try_into().ok()
}

fn i64_of(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The [`op_hash`] of stored envelope bytes. Bytes this build cannot decode
/// fall back to a hash of the bytes themselves: they reached the log through
/// a decoder once, so this is a damaged vault, and a deterministic answer is
/// better than one that stops the fold.
pub(super) fn hash_of_stored(bytes: &[u8]) -> [u8; 32] {
    decode_envelope(bytes)
        .ok()
        .and_then(|env| op_hash(&env).ok())
        // A plain BLAKE3 over the bytes, which is what `content_hash` is.
        .unwrap_or_else(|| sunrise_crypto::content_hash(bytes))
}

/// Record `ops.op_hash` for one row.
pub(super) fn set_op_hash(
    tx: &Transaction<'_>,
    op_id: &[u8; 16],
    hash: &[u8; 32],
) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE ops SET op_hash = ?1 WHERE op_id = ?2",
        params![&hash[..], &op_id[..]],
    )?;
    Ok(())
}

/// The [`op_hash`] of the op this replica holds at `(stream, device, seq)`,
/// or `None` if it holds none. A row from before migration 0034 has no stored
/// hash; it is computed from the envelope and written back.
pub(super) fn held_op_hash(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
    seq: u64,
) -> rusqlite::Result<Option<[u8; 32]>> {
    let key = params![&stream_id[..], &device_id[..], i64_of(seq)];
    let stored: Option<Option<Vec<u8>>> = tx
        .query_row(
            "SELECT op_hash FROM ops WHERE stream_id = ?1 AND device_id = ?2 AND seq = ?3",
            key,
            |r| r.get(0),
        )
        .optional()?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    if let Some(h) = stored.as_deref().and_then(to32) {
        return Ok(Some(h));
    }
    // Only a row from before migration 0034 reaches here, once.
    let (op_id, envelope): (Vec<u8>, Vec<u8>) = tx.query_row(
        "SELECT op_id, envelope FROM ops WHERE stream_id = ?1 AND device_id = ?2 AND seq = ?3",
        key,
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let h = hash_of_stored(&envelope);
    tx.execute(
        "UPDATE ops SET op_hash = ?1 WHERE op_id = ?2",
        params![&h[..], op_id],
    )?;
    Ok(Some(h))
}

/// Fields 14 and 15 for this device's op at `seq` in `stream_id` (ADR-0043
/// §1–§2), and the bookkeeping that keeps field 15 a delta.
///
/// Called inside the transaction that inserts the op, so a rollback takes the
/// `chain_heads_sent` rows back with it. At most [`MAX_CHAIN_HEADS`] heads are
/// listed, lowest device id first; a device left out is not marked listed, so
/// the next op lists it. Truncating would have weakened the causal claim
/// silently, and deferring does not.
pub(super) fn writer_links(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    me: &[u8; 16],
    seq: u64,
) -> rusqlite::Result<ChainLinks> {
    let prev_hash = if seq > 1 {
        held_op_hash(tx, stream_id, me, seq - 1)?
    } else {
        None
    };
    let tips: Vec<(Vec<u8>, i64)> = {
        let mut stmt = tx.prepare(
            "SELECT c.device_id, c.last_applied_seq FROM sync_cursors c
             LEFT JOIN chain_heads_sent h
               ON h.stream_id = c.stream_id AND h.device_id = c.device_id
             WHERE c.stream_id = ?1 AND c.device_id <> ?2
               AND c.last_applied_seq > COALESCE(h.seq, 0)
             ORDER BY c.device_id
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![&stream_id[..], &me[..], i64_of(MAX_CHAIN_HEADS as u64)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut heads = Vec::with_capacity(tips.len());
    for (device, n) in tips {
        let (Ok(device_id), Ok(seq)) = (<[u8; 16]>::try_from(device.as_slice()), u64::try_from(n))
        else {
            continue;
        };
        let Some(op_hash) = held_op_hash(tx, stream_id, &device_id, seq)? else {
            continue;
        };
        heads.push(ChainHead {
            device_id,
            seq,
            op_hash,
        });
        tx.execute(
            "INSERT INTO chain_heads_sent (stream_id, device_id, seq) VALUES (?1, ?2, ?3)
             ON CONFLICT(stream_id, device_id) DO UPDATE SET seq = excluded.seq",
            params![&stream_id[..], &device_id[..], n],
        )?;
    }
    Ok(ChainLinks { prev_hash, heads })
}

/// Record that `other_envelope` asserts `other_hash` for a position whose held
/// op hashes to `held_hash`. Returns whether the row is new.
#[allow(clippy::too_many_arguments)]
fn record_fork(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
    seq: u64,
    kind: ForkKind,
    held_hash: &[u8; 32],
    other_hash: &[u8; 32],
    other_envelope: &[u8],
    now_ms: u64,
) -> rusqlite::Result<bool> {
    let n = tx.execute(
        "INSERT OR IGNORE INTO fork_evidence
           (stream_id, device_id, seq, kind, held_hash, other_hash, other_envelope, recorded_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            &stream_id[..],
            &device_id[..],
            i64_of(seq),
            kind.as_str(),
            &held_hash[..],
            &other_hash[..],
            other_envelope,
            i64_of(now_ms),
        ],
    )?;
    if n > 0 {
        tracing::warn!(
            ev = "core.chain.fork",
            kind = kind.as_str(),
            stream_h = hex_short(stream_id),
            subject_h = hex_short(device_id),
            seq,
            "two ops signed by one device claim one position; both are kept"
        );
    }
    Ok(n > 0)
}

/// Record that the op at `(stream, device, seq)` must hash to `hash`, as the
/// op `named_by` asserts, when this replica does not hold it yet.
fn expect(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
    seq: u64,
    hash: &[u8; 32],
    named_by: &[u8; 16],
    reason: &'static str,
) -> rusqlite::Result<()> {
    let n = tx.execute(
        "INSERT OR IGNORE INTO chain_expected (stream_id, device_id, seq, op_hash, named_by)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            &stream_id[..],
            &device_id[..],
            i64_of(seq),
            &hash[..],
            &named_by[..]
        ],
    )?;
    if n > 0 {
        tracing::debug!(
            ev = "core.chain.missing",
            reason,
            stream_h = hex_short(stream_id),
            subject_h = hex_short(device_id),
            seq,
            "an op names an op this replica does not hold yet"
        );
    }
    Ok(())
}

/// Check the op that just entered the log against everything that names its
/// position, and everything it names (ADR-0043 §3). Records its `op_hash`.
///
/// `op_id` and `envelope_bytes` are the row just inserted; `env` is their
/// decoding. Never refuses: every outcome is a row of evidence or of
/// expectation, and the caller applies the op either way.
pub(super) fn check_links(
    tx: &Transaction<'_>,
    op_id: &[u8; 16],
    env: &OpEnvelope,
    envelope_bytes: &[u8],
    now_ms: u64,
) -> rusqlite::Result<()> {
    let hash = op_hash(env).map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
    set_op_hash(tx, op_id, &hash)?;
    let stream = env.stream_id;

    // What later ops said this one would be.
    let named: Vec<(Vec<u8>, Vec<u8>)> = {
        let mut stmt = tx.prepare(
            "SELECT op_hash, named_by FROM chain_expected
             WHERE stream_id = ?1 AND device_id = ?2 AND seq = ?3",
        )?;
        let rows = stmt.query_map(
            params![&stream[..], &env.device_id[..], i64_of(env.seq)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (expected, named_by) in named {
        let Some(expected) = to32(&expected) else {
            continue;
        };
        if expected == hash {
            continue;
        }
        let namer: Option<Vec<u8>> = tx
            .query_row(
                "SELECT envelope FROM ops WHERE op_id = ?1",
                params![named_by],
                |r| r.get(0),
            )
            .optional()?;
        let Some(namer) = namer else { continue };
        let kind = match decode_envelope(&namer) {
            Ok(n) if n.device_id == env.device_id && n.seq == env.seq.saturating_add(1) => {
                ForkKind::Link
            }
            _ => ForkKind::Head,
        };
        record_fork(
            tx,
            &stream,
            &env.device_id,
            env.seq,
            kind,
            &hash,
            &expected,
            &namer,
            now_ms,
        )?;
    }
    tx.execute(
        "DELETE FROM chain_expected WHERE stream_id = ?1 AND device_id = ?2 AND seq = ?3",
        params![&stream[..], &env.device_id[..], i64_of(env.seq)],
    )?;

    // What this op says its predecessor was.
    if let (Some(prev), true) = (env.prev_hash, env.seq > 1) {
        match held_op_hash(tx, &stream, &env.device_id, env.seq - 1)? {
            Some(held) if held != prev => {
                record_fork(
                    tx,
                    &stream,
                    &env.device_id,
                    env.seq - 1,
                    ForkKind::Link,
                    &held,
                    &prev,
                    envelope_bytes,
                    now_ms,
                )?;
            }
            Some(_) => {}
            None => expect(
                tx,
                &stream,
                &env.device_id,
                env.seq - 1,
                &prev,
                op_id,
                "prev",
            )?,
        }
    }

    // What this op says its writer had seen of the other devices.
    for head in &env.heads {
        if head.device_id == env.device_id || head.seq == 0 {
            continue;
        }
        match held_op_hash(tx, &stream, &head.device_id, head.seq)? {
            Some(held) if held != head.op_hash => {
                record_fork(
                    tx,
                    &stream,
                    &head.device_id,
                    head.seq,
                    ForkKind::Head,
                    &held,
                    &head.op_hash,
                    envelope_bytes,
                    now_ms,
                )?;
            }
            Some(_) => {}
            None => expect(
                tx,
                &stream,
                &head.device_id,
                head.seq,
                &head.op_hash,
                op_id,
                "head",
            )?,
        }
    }
    Ok(())
}

/// A verified envelope arrived for a `(stream, device, seq)` the log already
/// holds. If it is a different op, keep it as fork evidence (ADR-0043 §4).
/// Returns whether it was one.
pub(super) fn check_duplicate(
    tx: &Transaction<'_>,
    env: &OpEnvelope,
    envelope_bytes: &[u8],
    now_ms: u64,
) -> rusqlite::Result<bool> {
    let hash = op_hash(env).map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
    match held_op_hash(tx, &env.stream_id, &env.device_id, env.seq)? {
        Some(held) if held != hash => record_fork(
            tx,
            &env.stream_id,
            &env.device_id,
            env.seq,
            ForkKind::Seq,
            &held,
            &hash,
            envelope_bytes,
            now_ms,
        ),
        _ => Ok(false),
    }
}

/// `root(device, seq)` as stored, or `root(device, 0)` for `seq = 0`.
fn root_at(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
    seq: u64,
) -> rusqlite::Result<Option<[u8; 32]>> {
    if seq == 0 {
        return Ok(Some(chain_root_init(stream_id, device_id)));
    }
    let v: Option<Option<Vec<u8>>> = tx
        .query_row(
            "SELECT chain_root FROM ops WHERE stream_id = ?1 AND device_id = ?2 AND seq = ?3",
            params![&stream_id[..], &device_id[..], i64_of(seq)],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.flatten().as_deref().and_then(to32))
}

/// Extend `ops.chain_root` over `(stream, device)`'s contiguous prefix up to
/// `prefix`, then check every peer claim the prefix now reaches.
///
/// Roots are written in seq order from 1, so the highest stored root is where
/// the fold resumes: one step per op in steady state, and the whole prefix the
/// first time a device from before migration 0034 is folded.
pub(super) fn fold_chain(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
    prefix: u64,
    now_ms: u64,
) -> rusqlite::Result<()> {
    if prefix == 0 {
        return Ok(());
    }
    let resume: Option<(i64, Vec<u8>)> = tx
        .query_row(
            "SELECT seq, chain_root FROM ops
             WHERE stream_id = ?1 AND device_id = ?2 AND seq <= ?3 AND chain_root IS NOT NULL
             ORDER BY seq DESC LIMIT 1",
            params![&stream_id[..], &device_id[..], i64_of(prefix)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (mut seq, mut root) =
        match resume.and_then(|(s, r)| Some((u64::try_from(s).ok()?, to32(&r)?))) {
            Some(found) => found,
            None => (0, chain_root_init(stream_id, device_id)),
        };
    while seq < prefix {
        let Some(h) = held_op_hash(tx, stream_id, device_id, seq + 1)? else {
            break;
        };
        seq += 1;
        root = chain_root_step(&root, &h);
        tx.execute(
            "UPDATE ops SET chain_root = ?1 WHERE stream_id = ?2 AND device_id = ?3 AND seq = ?4",
            params![&root[..], &stream_id[..], &device_id[..], i64_of(seq)],
        )?;
    }

    let claims: Vec<(Vec<u8>, i64, Vec<u8>)> = {
        let mut stmt = tx.prepare(
            "SELECT claimed_by, seq, root FROM chain_claims
             WHERE stream_id = ?1 AND device_id = ?2 AND seq <= ?3",
        )?;
        let rows = stmt.query_map(params![&stream_id[..], &device_id[..], i64_of(seq)], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (peer, claim_seq, claim_root) in claims {
        tx.execute(
            "DELETE FROM chain_claims WHERE stream_id = ?1 AND device_id = ?2 AND claimed_by = ?3",
            params![&stream_id[..], &device_id[..], &peer],
        )?;
        let (Ok(peer), Ok(claim_seq), Some(claim_root)) = (
            <[u8; 16]>::try_from(peer.as_slice()),
            u64::try_from(claim_seq),
            to32(&claim_root),
        ) else {
            continue;
        };
        if let Some(held) = root_at(tx, stream_id, device_id, claim_seq)? {
            judge(
                tx,
                stream_id,
                device_id,
                &peer,
                claim_seq,
                &claim_root,
                &held,
                now_ms,
            )?;
        }
    }
    Ok(())
}

/// Compare a peer's root for `(stream, device)` at `seq` with this replica's
/// own, and keep `chain_divergence` in step: a disagreement is recorded, and
/// an agreement at or above a recorded disagreement clears it, because roots
/// chain and two replicas that agree at `n` agree at every seq below it.
#[allow(clippy::too_many_arguments)]
fn judge(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
    peer: &[u8; 16],
    seq: u64,
    peer_root: &[u8; 32],
    held_root: &[u8; 32],
    now_ms: u64,
) -> rusqlite::Result<()> {
    if peer_root == held_root {
        tx.execute(
            "DELETE FROM chain_divergence
             WHERE stream_id = ?1 AND device_id = ?2 AND peer_device_id = ?3 AND seq <= ?4",
            params![&stream_id[..], &device_id[..], &peer[..], i64_of(seq)],
        )?;
        return Ok(());
    }
    tx.execute(
        "INSERT INTO chain_divergence
           (stream_id, device_id, peer_device_id, seq, peer_root, held_root, recorded_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(stream_id, device_id, peer_device_id) DO UPDATE SET
           seq = excluded.seq, peer_root = excluded.peer_root,
           held_root = excluded.held_root, recorded_at_ms = excluded.recorded_at_ms",
        params![
            &stream_id[..],
            &device_id[..],
            &peer[..],
            i64_of(seq),
            &peer_root[..],
            &held_root[..],
            i64_of(now_ms),
        ],
    )?;
    tracing::warn!(
        ev = "core.chain.divergence",
        stream_h = hex_short(stream_id),
        subject_h = hex_short(device_id),
        sender_h = hex_short(peer),
        seq,
        "a peer holds different ops for a device than this replica does"
    );
    Ok(())
}

/// This replica's frontier in `stream_id`: each device with a non-empty
/// contiguous prefix, its end, and its chain root, sorted by device id.
pub(super) fn frontier(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    now_ms: u64,
) -> rusqlite::Result<Vec<FrontierEntry>> {
    let cursors: Vec<(Vec<u8>, i64)> = {
        let mut stmt = tx.prepare(
            "SELECT device_id, last_applied_seq FROM sync_cursors
             WHERE stream_id = ?1 AND last_applied_seq > 0 ORDER BY device_id",
        )?;
        let rows = stmt.query_map(params![&stream_id[..]], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut out = Vec::with_capacity(cursors.len());
    for (device, n) in cursors {
        let (Ok(device_id), Ok(seq)) = (<[u8; 16]>::try_from(device.as_slice()), u64::try_from(n))
        else {
            continue;
        };
        let root = match root_at(tx, stream_id, &device_id, seq)? {
            Some(r) => r,
            // A cursor written before migration 0034 and not moved since.
            None => {
                fold_chain(tx, stream_id, &device_id, seq, now_ms)?;
                match root_at(tx, stream_id, &device_id, seq)? {
                    Some(r) => r,
                    None => continue,
                }
            }
        };
        out.push(FrontierEntry {
            device_id,
            seq,
            root,
        });
    }
    Ok(out)
}

/// The [`StreamDigestPayload`] this replica would publish in `stream_id`, or
/// `None` while it holds no op there.
pub(super) fn digest_payload(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    now_ms: u64,
) -> rusqlite::Result<Option<StreamDigestPayload>> {
    let entries = frontier(tx, stream_id, now_ms)?;
    if entries.is_empty() {
        return Ok(None);
    }
    Ok(Some(StreamDigestPayload {
        digest: stream_digest(stream_id, &entries),
        frontier: entries
            .iter()
            .map(|e| FrontierWire(e.device_id, e.seq, e.root))
            .collect(),
        unknown: Unknowns::new(),
    }))
}

/// Compare a peer's digest of `stream_id` with this replica (ADR-0043 §5).
///
/// A payload whose `digest` is not the digest of its own frontier is damaged
/// and read for nothing. For each entry `(d, n, root)`:
///
/// - this replica holds `d` through `n`: its own `root(d, n)` is compared,
///   and a difference is a `chain_divergence`;
/// - it holds fewer: the entry is kept as a `chain_claims` row, which says it
///   is missing `(its n_d, n]` of `d`, and is checked when the prefix gets
///   there;
/// - `d` is this device, or the peer itself: compared like any other, since a
///   peer can hold a different copy of this device's chain only through a
///   fork or a relay that served two of them.
pub(super) fn reconcile(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    peer: &[u8; 16],
    payload: &StreamDigestPayload,
    now_ms: u64,
) -> rusqlite::Result<()> {
    let entries: Vec<FrontierEntry> = payload
        .frontier
        .iter()
        .map(|FrontierWire(device_id, seq, root)| FrontierEntry {
            device_id: *device_id,
            seq: *seq,
            root: *root,
        })
        .collect();
    let sorted_unique = entries.windows(2).all(|w| w[0].device_id < w[1].device_id);
    if !sorted_unique || stream_digest(stream_id, &entries) != payload.digest {
        tracing::warn!(
            ev = "core.chain.digest_invalid",
            stream_h = hex_short(stream_id),
            sender_h = hex_short(peer),
            "a stream digest does not match its own frontier; it was not compared"
        );
        return Ok(());
    }
    for e in entries {
        if e.seq == 0 {
            continue;
        }
        let held_seq: i64 = tx
            .query_row(
                "SELECT last_applied_seq FROM sync_cursors WHERE stream_id = ?1 AND device_id = ?2",
                params![&stream_id[..], &e.device_id[..]],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let held_seq = u64::try_from(held_seq).unwrap_or(0);
        if e.seq <= held_seq {
            if root_at(tx, stream_id, &e.device_id, e.seq)?.is_none() {
                fold_chain(tx, stream_id, &e.device_id, held_seq, now_ms)?;
            }
            if let Some(held) = root_at(tx, stream_id, &e.device_id, e.seq)? {
                judge(
                    tx,
                    stream_id,
                    &e.device_id,
                    peer,
                    e.seq,
                    &e.root,
                    &held,
                    now_ms,
                )?;
            }
        } else {
            let n = tx.execute(
                "INSERT INTO chain_claims (stream_id, device_id, claimed_by, seq, root)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(stream_id, device_id, claimed_by) DO UPDATE SET
                   seq = excluded.seq, root = excluded.root
                 WHERE excluded.seq > chain_claims.seq",
                params![
                    &stream_id[..],
                    &e.device_id[..],
                    &peer[..],
                    i64_of(e.seq),
                    &e.root[..]
                ],
            )?;
            if n > 0 {
                tracing::debug!(
                    ev = "core.chain.missing",
                    reason = "digest",
                    stream_h = hex_short(stream_id),
                    subject_h = hex_short(&e.device_id),
                    seq = e.seq,
                    "a peer holds ops of a device this replica does not"
                );
            }
        }
    }
    Ok(())
}

/// Publish a digest after this many ops have entered a stream's log since
/// this device's last one (ADR-0043 §5, taking over the "every 256 ops"
/// checkpoint rule of `docs/03-crypto/audit-and-tamper-evidence.md`).
pub(crate) const DIGEST_EVERY_OPS: u64 = 256;

/// ...or after this long, if anything entered it at all.
pub(crate) const DIGEST_EVERY_MS: u64 = 24 * 60 * 60 * 1000;

impl Engine {
    /// Publish this replica's digest of `stream_id` as a `StreamDigest` op in
    /// that stream, whatever the cadence says. Returns whether one was
    /// written: nothing is, while this replica holds no op in the stream or
    /// holds no key to seal under.
    ///
    /// # Errors
    /// Storage failures.
    pub fn publish_stream_digest(
        &self,
        db: &mut Db,
        stream_id: &[u8; 16],
    ) -> Result<bool, EngineError> {
        let now_ms = self.clock.now_ms();
        Ok(db.with_tx(|tx| self.emit_stream_digest(tx, stream_id, now_ms))?)
    }

    /// Publish a digest in every stream where one is due: no digest from this
    /// device yet and at least one op held, `DIGEST_EVERY_OPS` ops since
    /// the last, or `DIGEST_EVERY_MS` since the last with at least one op
    /// in between. Digest ops are not counted, so an idle account publishes
    /// nothing. Returns how many were written.
    ///
    /// # Errors
    /// Storage failures.
    pub fn publish_due_stream_digests(&self, db: &mut Db) -> Result<usize, EngineError> {
        let now_ms = self.clock.now_ms();
        let me = self.keychain.device_id();
        Ok(db.with_tx(|tx| -> rusqlite::Result<usize> {
            let streams: Vec<Vec<u8>> = {
                let mut stmt = tx.prepare(
                    "SELECT DISTINCT stream_id FROM sync_cursors WHERE last_applied_seq > 0
                     ORDER BY stream_id",
                )?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let mut written = 0;
            for s in streams {
                let Ok(stream_id) = <[u8; 16]>::try_from(s.as_slice()) else {
                    continue;
                };
                let last: Option<(i64, i64)> = tx
                    .query_row(
                        "SELECT rowid, ts_ms FROM ops
                         WHERE stream_id = ?1 AND device_id = ?2 AND inner_kind = ?3
                         ORDER BY seq DESC LIMIT 1",
                        params![&stream_id[..], &me[..], DIGEST_KIND],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                // Other devices' digests do not count: otherwise an idle
                // account's devices would answer each other's digests once a
                // day per stream, forever.
                let since: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM ops
                     WHERE stream_id = ?1 AND rowid > ?2 AND inner_kind <> ?3",
                    params![
                        &stream_id[..],
                        last.map_or(0, |(rowid, _)| rowid),
                        DIGEST_KIND
                    ],
                    |r| r.get(0),
                )?;
                let since = u64::try_from(since).unwrap_or(0);
                let due = match last {
                    None => since > 0,
                    Some((_, ts)) => {
                        since >= DIGEST_EVERY_OPS
                            || (since > 0
                                && now_ms.saturating_sub(u64::try_from(ts).unwrap_or(0))
                                    >= DIGEST_EVERY_MS)
                    }
                };
                if due && self.emit_stream_digest(tx, &stream_id, now_ms)? {
                    written += 1;
                }
            }
            Ok(written)
        })?)
    }

    /// What this replica holds about chains: fork evidence, digest
    /// disagreements, and ops it knows it is missing.
    ///
    /// # Errors
    /// Storage failures.
    pub fn chain_integrity(&self, db: &Db) -> Result<ChainIntegrity, EngineError> {
        Ok(integrity(db.conn())?)
    }

    fn emit_stream_digest(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        now_ms: u64,
    ) -> rusqlite::Result<bool> {
        // A stream this device holds no key for is one it cannot write to,
        // and minting a key here would hand the account a new epoch as a side
        // effect of a checksum.
        let Some((epoch, key)) = self.keychain.current_stream_key_tx(tx, stream_id)? else {
            return Ok(false);
        };
        let Some(payload) = digest_payload(tx, stream_id, now_ms)? else {
            return Ok(false);
        };
        let inner = InnerOp::StreamDigest(payload);
        let blob = encode_inner_op(&inner).map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
        let op_id = self.fresh_op_id(now_ms);
        let seq = self.next_seq_tx(tx, stream_id)?;
        let hlc = self.hlc.send();
        self.ops_insert_at(
            tx,
            &op_id,
            stream_id,
            seq,
            hlc,
            &blob,
            inner.inner_kind(),
            inner.target_kind(),
            None,
            Some(now_ms),
            None,
            now_ms,
            &[],
            epoch,
            &key,
        )?;
        Ok(true)
    }
}

/// The op-log `inner_kind` of a `StreamDigest`.
const DIGEST_KIND: &str = "stream.digest";

/// Counts for [`ChainIntegrity`].
pub(super) fn integrity(conn: &rusqlite::Connection) -> rusqlite::Result<ChainIntegrity> {
    conn.query_row(
        "SELECT (SELECT COUNT(*) FROM fork_evidence),
                (SELECT COUNT(*) FROM chain_divergence),
                (SELECT COUNT(*) FROM chain_expected) + (SELECT COUNT(*) FROM chain_claims)",
        [],
        |r| {
            let n = |i: usize| -> rusqlite::Result<u64> {
                Ok(u64::try_from(r.get::<_, i64>(i)?).unwrap_or(0))
            };
            Ok(ChainIntegrity {
                forks: n(0)?,
                divergences: n(1)?,
                wanted: n(2)?,
            })
        },
    )
}
