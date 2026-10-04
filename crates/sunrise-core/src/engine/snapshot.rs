//! The signed snapshot record, magic kind 4 (issue #330, ADR-0059,
//! `docs/04-storage/compaction.md` §Snapshot record).
//!
//! A snapshot is one stream's state at a stated causal frontier, sealed under
//! the stream's key and signed by the device that wrote it. A replica that
//! applies one holds what replaying every op through that frontier would have
//! given it, and catches up on the tail from the relay as usual.
//!
//! # Wire shape
//!
//! ```text
//! record = "SR" 0x04 version(u16 BE = 1) || canonical CBOR map
//! ```
//!
//! | Key | Field | |
//! |---|---|---|
//! | 1 | `stream_id` | `bstr .size 16` |
//! | 2 | `epoch` | the stream-key epoch the body is sealed under |
//! | 3 | `generated_by` | the writer's device id |
//! | 4 | `generated_at_ms` | the writer's wall clock |
//! | 5 | `frontier` | `[* [device, seq, root, op_hash, hlc_ms, hlc_logical]]`, sorted by device |
//! | 6 | `digest` | ADR-0043's stream digest of `(device, seq, root)` |
//! | 7 | `cert` | the writer's identity-signed `DeviceCert` |
//! | 8 | `nonce` | 24 random bytes |
//! | 9 | `body` | XChaCha20-Poly1305 of the canonical CBOR body |
//! | 10 | `sig` | Ed25519 by the writer's device key |
//!
//! The body is `{1: doc_state, 2: [* retained envelope]}`: the merge state of
//! [`super::merge::dump_stream_state`], and every op of the stream at or below
//! the frontier whose effect is not in that state, verbatim (control ops,
//! focus and review records, parked ops of kinds this build does not know).
//! The AEAD key is the stream key at `epoch` run through the BLAKE3 KDF under
//! [`KEY_CONTEXT`]; the AAD is [`AAD_CONTEXT`], the five magic bytes and the
//! canonical encoding of fields 1 to 8. The signature covers [`SIG_CONTEXT`],
//! the magic bytes and the canonical encoding of fields 1 to 9.
//!
//! # What a reader checks, in order
//!
//! 1. The magic prefix and the format version.
//! 2. The signature, under the writer's cert as this vault holds it, or under
//!    the carried cert once it verifies under an identity on this account's
//!    chain. A writer this replica holds as revoked is refused.
//! 3. The digest is the digest of the frontier, and the frontier is sorted.
//! 4. The body opens under a key this replica holds at `epoch`.
//! 5. Every frontier entry this replica's own prefix reaches agrees with its
//!    own chain root there. A snapshot that disagrees is of a different
//!    history, and is refused.
//!
//! The snapshot's state is attested by its writer's signature and by nothing
//! finer: the ops it folded are gone, so no single write in it can be
//! re-verified. That is the trust compaction trades for bounded storage, and
//! the reason only a current member's snapshot is accepted.

use ciborium::value::Value;
use rusqlite::{params, OptionalExtension, Transaction};
use sunrise_cbor::hlc::Hlc;
use sunrise_cbor::magic::{decode_prefix, write_prefix, MagicKind, MAGIC_LEN};
use sunrise_crypto::blake3_kdf::derive_key_32;
use sunrise_crypto::keys::verify_ed25519;
use sunrise_crypto::{
    aead_open_xchacha, aead_seal_xchacha, decode_envelope, stream_digest, DeviceCert,
    FrontierEntry, StreamKey, AEAD_NONCE_LEN,
};
use sunrise_storage::{Db, DbError};

use super::chain::{frontier, held_op_hash, root_at, settle_below_floor};
use super::compaction::{compactable_kinds, floor_of, raise_floor, Floor};
use super::ids::hex_short;
use super::merge::{dump_stream_state, fold_rows, join_stream_state};
use super::oplog::upsert_sync_cursor;
use super::{Engine, EngineError};
use crate::events::DomainEvent;

/// The snapshot record's format version, carried in its magic prefix.
pub const SNAPSHOT_FORMAT_V: u16 = 1;

/// KDF context that turns a stream key into a snapshot body key.
const KEY_CONTEXT: &str = "sunrise.snapshot.key.v1";
/// Domain separator for the body's AAD.
const AAD_CONTEXT: &[u8] = b"sunrise.snapshot.aad.v1";
/// Domain separator for the writer's signature.
const SIG_CONTEXT: &[u8] = b"sunrise.snapshot.sig.v1";

/// What [`Engine::apply_snapshot`] did with a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotApplied {
    /// The state was joined in and the stream's floors rose to the frontier.
    Applied {
        /// Entities re-projected from the joined state.
        entities: usize,
        /// Retained ops the record carried.
        retained: usize,
    },
    /// This replica's own prefix already reaches every frontier entry, so the
    /// record holds nothing it lacks. Nothing was written.
    Covered,
    /// No key this replica holds at the record's epoch. Nothing was written;
    /// apply it again once the key has arrived.
    NoKey,
    /// Some retained ops could not be applied yet, because the key they are
    /// sealed under has not arrived. They wait in the deferred queue and
    /// the floors were not raised; apply the record again once they are in.
    Pending {
        /// Retained ops still missing from the op log.
        missing: usize,
    },
}

fn invalid(why: &str) -> EngineError {
    EngineError::Invalid(format!("snapshot: {why}"))
}

fn int(n: u64) -> Value {
    Value::Integer(n.into())
}

fn key(n: u64) -> Value {
    int(n)
}

fn as_u64(v: &Value) -> Option<u64> {
    u64::try_from(v.as_integer()?).ok()
}

fn fixed<const N: usize>(v: &Value) -> Option<[u8; N]> {
    v.as_bytes()?.as_slice().try_into().ok()
}

fn encode(v: &Value) -> Result<Vec<u8>, EngineError> {
    sunrise_cbor::encode_canonical(v).map_err(|e| EngineError::Cbor(e.to_string()))
}

fn prefix() -> [u8; MAGIC_LEN] {
    let mut p = [0u8; MAGIC_LEN];
    write_prefix(&mut p, MagicKind::Snapshot, SNAPSHOT_FORMAT_V);
    p
}

/// `context || magic || canonical(fields)`: what the AAD and the signature
/// are computed over.
fn bound(context: &[u8], fields: &[(Value, Value)]) -> Result<Vec<u8>, EngineError> {
    let mut out = Vec::with_capacity(context.len() + MAGIC_LEN + 256);
    out.extend_from_slice(context);
    out.extend_from_slice(&prefix());
    out.extend_from_slice(&encode(&Value::Map(fields.to_vec()))?);
    Ok(out)
}

fn body_key(stream_key: &StreamKey) -> [u8; 32] {
    derive_key_32(KEY_CONTEXT, stream_key.as_bytes())
}

/// One frontier entry: the device's prefix end, its chain root, the op hash
/// at that seq and that op's stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    device: [u8; 16],
    floor: Floor,
}

impl Entry {
    fn to_value(self) -> Value {
        Value::Array(vec![
            Value::Bytes(self.device.to_vec()),
            int(self.floor.seq),
            Value::Bytes(self.floor.root.to_vec()),
            Value::Bytes(self.floor.op_hash.to_vec()),
            int(self.floor.hlc.physical_ms),
            int(u64::from(self.floor.hlc.logical)),
        ])
    }

    fn from_value(v: &Value) -> Option<Self> {
        let [device, seq, root, op_hash, ms, logical] = v.as_array()?.as_slice() else {
            return None;
        };
        Some(Self {
            device: fixed(device)?,
            floor: Floor {
                seq: as_u64(seq)?,
                op_hash: fixed(op_hash)?,
                root: fixed(root)?,
                hlc: Hlc {
                    physical_ms: as_u64(ms)?,
                    logical: u32::try_from(as_u64(logical)?).ok()?,
                },
            },
        })
    }

    fn frontier_entry(self) -> FrontierEntry {
        FrontierEntry {
            device_id: self.device,
            seq: self.floor.seq,
            root: self.floor.root,
        }
    }
}

/// A record whose outer fields have been read and nothing checked yet.
struct Record {
    fields: Vec<(Value, Value)>,
    stream_id: [u8; 16],
    epoch: u32,
    generated_by: [u8; 16],
    generated_at_ms: u64,
    entries: Vec<Entry>,
    digest: [u8; 32],
    cert: Vec<u8>,
    nonce: [u8; AEAD_NONCE_LEN],
    body: Vec<u8>,
    sig: [u8; 64],
}

fn parse(record: &[u8]) -> Result<Record, EngineError> {
    let magic = decode_prefix(record).map_err(|_| invalid("bad magic"))?;
    if magic.kind != MagicKind::Snapshot {
        return Err(invalid("not a snapshot record"));
    }
    if magic.version != SNAPSHOT_FORMAT_V {
        return Err(invalid("unknown format version"));
    }
    let map: Value = sunrise_cbor::decode_canonical(&record[MAGIC_LEN..])
        .map_err(|_| invalid("not canonical CBOR"))?;
    let Value::Map(fields) = map else {
        return Err(invalid("not a map"));
    };
    let field = |n: u64| {
        fields
            .iter()
            .find(|(k, _)| as_u64(k) == Some(n))
            .map(|(_, v)| v)
            .ok_or_else(|| invalid("missing field"))
    };
    let entries = field(5)?
        .as_array()
        .ok_or_else(|| invalid("frontier"))?
        .iter()
        .map(Entry::from_value)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| invalid("frontier entry"))?;
    Ok(Record {
        stream_id: fixed(field(1)?).ok_or_else(|| invalid("stream_id"))?,
        epoch: as_u64(field(2)?)
            .and_then(|e| u32::try_from(e).ok())
            .ok_or_else(|| invalid("epoch"))?,
        generated_by: fixed(field(3)?).ok_or_else(|| invalid("generated_by"))?,
        generated_at_ms: as_u64(field(4)?).ok_or_else(|| invalid("generated_at_ms"))?,
        entries,
        digest: fixed(field(6)?).ok_or_else(|| invalid("digest"))?,
        cert: field(7)?
            .as_bytes()
            .cloned()
            .ok_or_else(|| invalid("cert"))?,
        nonce: fixed(field(8)?).ok_or_else(|| invalid("nonce"))?,
        body: field(9)?
            .as_bytes()
            .cloned()
            .ok_or_else(|| invalid("body"))?,
        sig: fixed(field(10)?).ok_or_else(|| invalid("sig"))?,
        fields,
    })
}

impl Record {
    /// Fields `1..=n`, in key order, for the AAD (`n = 8`) and the signature
    /// (`n = 9`).
    fn through(&self, n: u64) -> Vec<(Value, Value)> {
        let mut out: Vec<(Value, Value)> = self
            .fields
            .iter()
            .filter(|(k, _)| as_u64(k).is_some_and(|k| (1..=n).contains(&k)))
            .cloned()
            .collect();
        out.sort_by_key(|(k, _)| as_u64(k));
        out
    }
}

impl Engine {
    /// Write a snapshot of `stream_id` at this replica's frontier, store it as
    /// the stream's latest, and return the record. `None` when there is
    /// nothing to write or no frontier to state: no key to seal under, no op
    /// held in the stream, or an op held above its device's contiguous
    /// prefix, whose state is in the merge but whose position no frontier can
    /// name.
    ///
    /// [`Self::compact_op_log`] calls this for the streams this replica is the
    /// compactor of. It is public so a transport, or a test, can ask for one.
    ///
    /// # Errors
    /// Storage failures.
    pub fn write_stream_snapshot(
        &self,
        db: &mut Db,
        stream_id: &[u8; 16],
    ) -> Result<Option<Vec<u8>>, EngineError> {
        let now_ms = self.clock.now_ms();
        let me = self.keychain.device_id();
        let record = in_tx(db, |tx| self.snapshot_record(tx, stream_id, now_ms))?;
        if record.is_some() {
            tracing::info!(
                ev = "core.snapshot.written",
                stream_h = hex_short(stream_id),
                sender_h = hex_short(&me),
                "a snapshot of the stream was written at this replica's frontier"
            );
        }
        Ok(record)
    }

    fn snapshot_record(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        let Some((epoch, stream_key)) = self.keychain.current_stream_key_tx(tx, stream_id)? else {
            return Ok(None);
        };
        let above: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM ops o
                 LEFT JOIN sync_cursors c
                   ON c.stream_id = o.stream_id AND c.device_id = o.device_id
                 WHERE o.stream_id = ?1 AND o.seq > COALESCE(c.last_applied_seq, 0)
                 LIMIT 1",
                params![&stream_id[..]],
                |r| r.get(0),
            )
            .optional()?;
        if above.is_some() {
            tracing::debug!(
                ev = "core.snapshot.skipped",
                reason = "gap",
                stream_h = hex_short(stream_id),
                "an op is held above its device's prefix; no frontier states this replica"
            );
            return Ok(None);
        }
        let mut entries = Vec::new();
        for e in frontier(tx, stream_id, now_ms)? {
            let Some(op_hash) = held_op_hash(tx, stream_id, &e.device_id, e.seq)? else {
                return Ok(None);
            };
            let row: Option<(Vec<u8>, i64)> = tx
                .query_row(
                    "SELECT envelope, ts_ms FROM ops
                     WHERE stream_id = ?1 AND device_id = ?2 AND seq = ?3",
                    params![
                        &stream_id[..],
                        &e.device_id[..],
                        i64::try_from(e.seq).unwrap_or(i64::MAX)
                    ],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let hlc = match row {
                Some((envelope, ts_ms)) => decode_envelope(&envelope).map_or(
                    Hlc {
                        physical_ms: u64::try_from(ts_ms).unwrap_or(0),
                        logical: 0,
                    },
                    |env| env.hlc,
                ),
                None => match floor_of(tx, stream_id, &e.device_id)? {
                    Some(f) if f.seq == e.seq => f.hlc,
                    _ => return Ok(None),
                },
            };
            entries.push(Entry {
                device: e.device_id,
                floor: Floor {
                    seq: e.seq,
                    op_hash,
                    root: e.root,
                    hlc,
                },
            });
        }
        if entries.is_empty() {
            return Ok(None);
        }

        let kinds = compactable_kinds();
        let in_list = kinds
            .iter()
            .map(|k| format!("'{k}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let targets: Vec<(String, Vec<u8>)> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT DISTINCT target_kind, target_id FROM ops
                 WHERE stream_id = ?1 AND target_id IS NOT NULL AND inner_kind IN ({in_list})"
            ))?;
            let rows = stmt.query_map(params![&stream_id[..]], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        fold_rows(tx, &targets)?;
        let doc_state = dump_stream_state(tx, stream_id)?;
        let retained: Vec<Value> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT envelope FROM ops
                 WHERE stream_id = ?1
                   AND (inner_kind NOT IN ({in_list})
                        OR op_id IN (SELECT op_id FROM parked_ops))
                 ORDER BY ts_ms, device_id, seq"
            ))?;
            let rows = stmt.query_map(params![&stream_id[..]], |r| r.get::<_, Vec<u8>>(0))?;
            rows.map(|r| r.map(Value::Bytes))
                .collect::<rusqlite::Result<_>>()?
        };
        let body = encode(&Value::Map(vec![
            (key(1), doc_state),
            (key(2), Value::Array(retained)),
        ]))?;

        let me = self.keychain.device_id();
        let digest = stream_digest(
            stream_id,
            &entries
                .iter()
                .map(|e| e.frontier_entry())
                .collect::<Vec<_>>(),
        );
        let mut nonce = [0u8; AEAD_NONCE_LEN];
        self.rng.fill_bytes(&mut nonce);
        let mut fields = vec![
            (key(1), Value::Bytes(stream_id.to_vec())),
            (key(2), int(u64::from(epoch))),
            (key(3), Value::Bytes(me.to_vec())),
            (key(4), int(now_ms)),
            (
                key(5),
                Value::Array(entries.iter().map(|e| e.to_value()).collect()),
            ),
            (key(6), Value::Bytes(digest.to_vec())),
            (key(7), Value::Bytes(self.keychain.cert_blob())),
            (key(8), Value::Bytes(nonce.to_vec())),
        ];
        let sealed = aead_seal_xchacha(
            &body_key(&stream_key),
            &nonce,
            &body,
            &bound(AAD_CONTEXT, &fields)?,
        )
        .map_err(|_| invalid("the body could not be sealed"))?;
        fields.push((key(9), Value::Bytes(sealed)));
        let sig = self.keychain.sign_device(&bound(SIG_CONTEXT, &fields)?);
        fields.push((key(10), Value::Bytes(sig.to_vec())));
        let mut record = prefix().to_vec();
        record.extend_from_slice(&encode(&Value::Map(fields))?);
        store(tx, stream_id, &me, now_ms, &digest, &record)?;
        Ok(Some(record))
    }

    /// The latest snapshot record of `stream_id` this replica wrote or
    /// applied, verbatim.
    ///
    /// # Errors
    /// Storage failures.
    pub fn stream_snapshot(
        &self,
        db: &Db,
        stream_id: &[u8; 16],
    ) -> Result<Option<Vec<u8>>, EngineError> {
        Ok(db
            .conn()
            .query_row(
                "SELECT record FROM stream_snapshots WHERE stream_id = ?1",
                params![&stream_id[..]],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The signing key of a snapshot's writer: its cert as this vault holds
    /// it, or the carried cert once it verifies under an identity on this
    /// account's chain, as a self-published cert would.
    fn snapshot_signer(&self, db: &Db, rec: &Record) -> Result<[u8; 32], EngineError> {
        let cert = match self.lookup_device_cert(db, &rec.generated_by)? {
            Some(held) => DeviceCert::from_cbor(&held).map_err(|_| invalid("stored cert"))?,
            None => {
                let cert = DeviceCert::from_cbor(&rec.cert).map_err(|_| invalid("cert"))?;
                let chain = self.chain_identities(db.conn())?;
                if !chain
                    .iter()
                    .any(|(id, pk)| cert.verify_binding(pk, id).is_ok())
                {
                    return Err(invalid("the writer's cert verifies under no identity here"));
                }
                cert
            }
        };
        if cert.body.device_id != rec.generated_by {
            return Err(invalid("the cert names another device"));
        }
        Ok(cert.body.d_s_pub)
    }

    /// Apply a snapshot record: check it (see the module docs), apply the
    /// retained ops it carries through the ordinary receive path, then join
    /// its state into this replica's and raise each frontier device's floor
    /// to its entry. Returns what happened and the events the caller would
    /// broadcast.
    ///
    /// Applying a record twice, or one older than what this replica holds, is
    /// harmless: the join is idempotent and a floor never falls.
    ///
    /// # Errors
    /// [`EngineError::Invalid`] for a record that is malformed, forged, from a
    /// revoked writer, or of a history that disagrees with this replica's;
    /// storage failures.
    pub fn apply_snapshot(
        &self,
        db: &mut Db,
        record: &[u8],
    ) -> Result<(SnapshotApplied, Vec<DomainEvent>), EngineError> {
        let rec = parse(record)?;
        let d_s_pub = self.snapshot_signer(db, &rec)?;
        if !verify_ed25519(&d_s_pub, &bound(SIG_CONTEXT, &rec.through(9))?, &rec.sig) {
            return Err(invalid("bad signature"));
        }
        if self.is_revoked(db.conn(), &rec.generated_by)? {
            return Err(invalid("the writer is revoked"));
        }
        let frontier: Vec<FrontierEntry> = rec.entries.iter().map(|e| e.frontier_entry()).collect();
        let sorted = frontier.windows(2).all(|w| w[0].device_id < w[1].device_id);
        if !sorted || stream_digest(&rec.stream_id, &frontier) != rec.digest {
            return Err(invalid("the digest is not the digest of the frontier"));
        }
        let keys = self.keychain.stream_keys_at(&rec.stream_id, rec.epoch);
        if keys.is_empty() {
            return Ok((SnapshotApplied::NoKey, Vec::new()));
        }
        let aad = bound(AAD_CONTEXT, &rec.through(8))?;
        let body = keys
            .iter()
            .find_map(|k| aead_open_xchacha(&body_key(k), &rec.nonce, &rec.body, &aad).ok())
            .ok_or_else(|| invalid("no key at this epoch opens the body"))?;
        let body: Value =
            sunrise_cbor::decode_canonical(&body).map_err(|_| invalid("body is not canonical"))?;
        let body = body.as_map().ok_or_else(|| invalid("body"))?;
        let part = |n: u64| {
            body.iter()
                .find(|(k, _)| as_u64(k) == Some(n))
                .map(|(_, v)| v)
                .ok_or_else(|| invalid("body field"))
        };
        let doc_state = part(1)?.clone();
        let retained: Vec<Vec<u8>> = part(2)?
            .as_array()
            .ok_or_else(|| invalid("retained"))?
            .iter()
            .map(|v| v.as_bytes().cloned())
            .collect::<Option<_>>()
            .ok_or_else(|| invalid("retained op"))?;

        // 5. Against this replica's own history, and whether it adds anything.
        let now_ms = self.clock.now_ms();
        let adds = in_tx(db, |tx| {
            let mut adds = false;
            for e in &rec.entries {
                let held: i64 = tx
                    .query_row(
                        "SELECT last_applied_seq FROM sync_cursors
                         WHERE stream_id = ?1 AND device_id = ?2",
                        params![&rec.stream_id[..], &e.device[..]],
                        |r| r.get(0),
                    )
                    .optional()?
                    .unwrap_or(0);
                let held = u64::try_from(held).unwrap_or(0);
                if held < e.floor.seq {
                    adds = true;
                    continue;
                }
                if root_at(tx, &rec.stream_id, &e.device, e.floor.seq)?
                    .is_some_and(|r| r != e.floor.root)
                {
                    return Err(invalid("its frontier disagrees with this replica's chain"));
                }
            }
            Ok(adds)
        })?;
        if !adds {
            return Ok((SnapshotApplied::Covered, Vec::new()));
        }
        if let Some(max) = rec.entries.iter().map(|e| e.floor.hlc).max() {
            self.hlc
                .observe(max)
                .map_err(|e| invalid(&format!("hlc: {e}")))?;
        }

        // The retained ops go through the receive path, which verifies each
        // one. A cert arrives in the same list as the ops it signs, so what an
        // unknown device refused is retried once the rest are in.
        let mut events = Vec::new();
        let mut pending = retained.clone();
        loop {
            let mut next = Vec::new();
            for env in &pending {
                match self.apply_remote_all(db, env) {
                    Ok(ev) => events.extend(ev),
                    Err(EngineError::UnknownDevice) => next.push(env.clone()),
                    Err(e) => return Err(e),
                }
            }
            if next.is_empty() || next.len() == pending.len() {
                if !next.is_empty() {
                    return Err(invalid(
                        "a retained op names a device this account never had",
                    ));
                }
                break;
            }
            pending = next;
        }
        let mut missing = 0;
        for env in &retained {
            let env = decode_envelope(env).map_err(|_| invalid("retained op"))?;
            let held: Option<i64> = db
                .conn()
                .query_row(
                    "SELECT 1 FROM ops WHERE stream_id = ?1 AND device_id = ?2 AND seq = ?3",
                    params![
                        &env.stream_id[..],
                        &env.device_id[..],
                        i64::try_from(env.seq).unwrap_or(i64::MAX)
                    ],
                    |r| r.get(0),
                )
                .optional()?;
            if held.is_none() {
                missing += 1;
            }
        }
        if missing > 0 {
            return Ok((SnapshotApplied::Pending { missing }, events));
        }

        let stream_id = rec.stream_id;
        let joined = in_tx(db, |tx| {
            let entities = join_stream_state(tx, &doc_state)?;
            for e in &rec.entries {
                if raise_floor(tx, &stream_id, &e.device, &e.floor)? {
                    settle_below_floor(tx, &stream_id, &e.device, &e.floor, now_ms)?;
                }
                upsert_sync_cursor(tx, &stream_id, &e.device, now_ms)?;
            }
            store(
                tx,
                &stream_id,
                &rec.generated_by,
                rec.generated_at_ms,
                &rec.digest,
                record,
            )?;
            events.extend(entities.iter().map(|t| DomainEvent::Updated(*t)));
            Ok(entities.len())
        })?;
        tracing::info!(
            ev = "core.snapshot.applied",
            stream_h = hex_short(&stream_id),
            sender_h = hex_short(&rec.generated_by),
            n_retained = retained.len(),
            "a snapshot was joined and the stream's floors rose to its frontier"
        );
        Ok((
            SnapshotApplied::Applied {
                entities: joined,
                retained: retained.len(),
            },
            events,
        ))
    }
}

/// Run `f` in one transaction, rolling it back on any error and carrying an
/// [`EngineError`] out as itself rather than as the storage error the
/// transaction closure has to speak.
fn in_tx<R>(
    db: &mut Db,
    f: impl FnOnce(&Transaction<'_>) -> Result<R, EngineError>,
) -> Result<R, EngineError> {
    db.with_tx(|tx| {
        f(tx).map_err(|e| match e {
            EngineError::Sqlite(e) => e,
            other => rusqlite::Error::ToSqlConversionFailure(Box::new(other)),
        })
    })
    .map_err(|e| match e {
        DbError::Sqlite(rusqlite::Error::ToSqlConversionFailure(b)) => {
            match b.downcast::<EngineError>() {
                Ok(e) => *e,
                Err(b) => EngineError::Invalid(b.to_string()),
            }
        }
        other => other.into(),
    })
}

/// Keep `record` as `stream_id`'s latest snapshot, unless the one held was
/// written later.
fn store(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    generated_by: &[u8; 16],
    generated_at_ms: u64,
    digest: &[u8; 32],
    record: &[u8],
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO stream_snapshots (stream_id, generated_by, generated_at_ms, digest, record)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(stream_id) DO UPDATE SET
           generated_by = excluded.generated_by, generated_at_ms = excluded.generated_at_ms,
           digest = excluded.digest, record = excluded.record
         WHERE excluded.generated_at_ms >= stream_snapshots.generated_at_ms",
        params![
            &stream_id[..],
            &generated_by[..],
            i64::try_from(generated_at_ms).unwrap_or(i64::MAX),
            &digest[..],
            record
        ],
    )?;
    Ok(())
}
