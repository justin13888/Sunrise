//! Op-log, outbox, stream-epoch and sync-cursor writes.
//!
//! Every function here writes `ops`, `outbox`, `stream_epochs` or
//! `sync_cursors`, and none of them knows what an entity is: each is handed an
//! already-encoded inner op and a routing stream id. This is the only module
//! that names `OpLog` or `Outbox` from `sunrise_storage`, which is what keeps
//! "one op, one envelope, one outbox row, inside the caller's transaction"
//! checkable in one place.

use crate::control_op::{KeyEnvelopePayload, Recipient};
use crate::inner_op::{encode_inner_op, InnerOp};
use crate::keychain::{to16, Keychain};
use rusqlite::{params, OptionalExtension, Transaction};
use sunrise_cbor::hlc::Hlc;
use sunrise_crypto::{stream_key_id, StreamKey};
use sunrise_domain::INBOX_STREAM_BYTES;
use sunrise_storage::{Db, OpLog, Outbox};
// `open_op_row` is the only user and is `#[cfg(test)]`.
#[cfg(test)]
use super::ids::hex_short;
use super::{Engine, EngineError, META_STREAM};

impl Engine {
    /// Seal `inner_op` into a real [`sunrise_crypto::OpEnvelope`] under the routing stream's
    /// **live** epoch and append it to the op log, enqueueing it in the outbox
    /// — all inside the caller's transaction.
    ///
    /// `stream_id` is the op's *routing* stream (the meta stream for
    /// Stream/routine/control ops; the owning Stream for task ops); it is bound
    /// into the envelope and the outbox row. The signing device id is taken
    /// from the keychain, so envelope `seq` is per `(stream_id, device_id)`.
    ///
    /// If the stream has no key yet this mints epoch 1 and emits the
    /// `key_envelope` ops that give every other member of the account a copy —
    /// see [`Self::ensure_stream_epoch`]. That is the whole reason a key can
    /// never be minted silently: an epoch nobody was told about is an epoch
    /// whose ops nobody else can read.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn ops_insert(
        &self,
        tx: &Transaction<'_>,
        op_id: &[u8; 16],
        stream_id: &[u8; 16],
        seq: u64,
        hlc: Hlc,
        inner_op: &[u8],
        inner_kind: &str,
        target_kind: &str,
        target_id: Option<&[u8; 16]>,
        applied_at_ms: Option<u64>,
        received_from: Option<&[u8; 16]>,
        received_at_ms: u64,
        deps: &[[u8; 16]],
    ) -> rusqlite::Result<()> {
        let (epoch, key) = self.ensure_stream_epoch(tx, stream_id, hlc.physical_ms)?;
        self.ops_insert_at(
            tx,
            op_id,
            stream_id,
            seq,
            hlc,
            inner_op,
            inner_kind,
            target_kind,
            target_id,
            applied_at_ms,
            received_from,
            received_at_ms,
            deps,
            epoch,
            &key,
        )
    }

    /// [`Self::ops_insert`] with the seal epoch chosen by the caller.
    ///
    /// A rotation needs this. The ops that *carry* the new keys have to be
    /// sealed under the **old** epoch: a device that does not yet hold the new
    /// key could not otherwise read the op that gives it one. Sealing them
    /// under the new epoch would be a deadlock — and one that only shows up on
    /// the second device, weeks later.
    ///
    /// A revoked device can read those rotation ops, because it still holds
    /// the old epoch. It cannot read the keys inside them. Its own
    /// `Recipient::Device` copy is not emitted — [`Self::emit_key_envelopes`]
    /// drops it from the list — and the identity copy sealed alongside is no
    /// longer openable by a device pairing admitted, because `PairingPayload`
    /// stopped carrying `ID_D_priv`. That pair is `#76`, and it is the whole of
    /// the read bound: either half alone is vacuous.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn ops_insert_at(
        &self,
        tx: &Transaction<'_>,
        op_id: &[u8; 16],
        stream_id: &[u8; 16],
        seq: u64,
        hlc: Hlc,
        inner_op: &[u8],
        inner_kind: &str,
        target_kind: &str,
        target_id: Option<&[u8; 16]>,
        applied_at_ms: Option<u64>,
        received_from: Option<&[u8; 16]>,
        received_at_ms: u64,
        deps: &[[u8; 16]],
        epoch: u32,
        stream_key: &StreamKey,
    ) -> rusqlite::Result<()> {
        let device_id = self.keychain.device_id();
        let ts_ms = hlc.physical_ms;
        let envelope = self
            .keychain
            .seal_op_at(
                *stream_id,
                seq,
                hlc,
                inner_op,
                self.rng.as_ref(),
                epoch,
                stream_key,
            )
            .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
        if let Err(e) = OpLog::insert(
            tx,
            op_id,
            stream_id,
            &device_id,
            seq,
            ts_ms,
            &envelope,
            inner_kind,
            target_kind,
            target_id,
            applied_at_ms,
            received_from,
            received_at_ms,
            deps,
        ) {
            return match e {
                sunrise_storage::OpLogError::Sqlite(s) => Err(s),
                sunrise_storage::OpLogError::Db(_) => Err(rusqlite::Error::ExecuteReturnedResults),
            };
        }
        // Same-transaction outbox enqueue: the pending marker commits with the op.
        Outbox::enqueue(tx, op_id, stream_id, ts_ms)
            .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
        // A device has certainly applied its own ops, so its own cursor belongs
        // in `sync_cursors` alongside every peer's. Without it the Subscribe
        // frame claims nothing about this device, and the relay — which filters
        // replay by those cursors — hands the device its entire own history
        // back on every reconnect for it to re-dedupe.
        upsert_sync_cursor(tx, stream_id, &device_id)?;
        Ok(())
    }

    /// Mint the account's base epochs if they do not exist yet.
    ///
    /// A pairing payload is built from the keys this device *holds*, and a
    /// vault that has never written anything holds none — so a device paired
    /// from a freshly created account used to receive an empty `stream_keys`
    /// map. That was survivable only because it could open the identity-sealed
    /// copy of every `key_envelope` with the `ID_D_priv` the payload also
    /// carried. Neither is true now: without the vault-meta key a paired device
    /// cannot read a single control op, so it cannot even learn the keys it is
    /// missing, and it sits in `CatchingUp` forever parking everything.
    ///
    /// The Inbox is minted alongside because it is the one stream every account
    /// has whether or not the user has made any of their own.
    ///
    /// `Core::open` is the only caller, and that placement is the point.
    /// `Core::export_pairing_payload` called it for one revision, which is
    /// where the need was discovered; it made opening a pairing screen a
    /// durable write that syncs, on a call every layer above had written
    /// against as a read. Having base epochs is an invariant of a vault rather
    /// than a fact about pairing, so it is established when the vault is
    /// opened. This is idempotent — `ensure_stream_epoch` returns the existing
    /// key when there is one — so it also repairs a vault created before the
    /// move, and mints nothing on a device that imported the account's epochs
    /// from a pairing payload.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn ensure_base_epochs(&self, db: &mut Db) -> Result<(), EngineError> {
        let now_ms = self.clock.now_ms();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            self.ensure_stream_epoch(tx, &META_STREAM, now_ms)?;
            self.ensure_stream_epoch(tx, &INBOX_STREAM_BYTES, now_ms)?;
            Ok(())
        })
        .map_err(EngineError::Storage)
    }

    /// The live `(epoch, key)` for `stream_id`, minting epoch 1 and telling
    /// every other member about it if the stream has none.
    ///
    /// The order inside the mint branch matters and is not incidental: the key
    /// row is written **before** the `key_envelope` ops are emitted, because
    /// emitting one is itself an `ops_insert` into the vault-meta stream, which
    /// re-enters here. With the row already present the re-entry terminates
    /// immediately; without it, minting the meta stream's own first key would
    /// recurse forever.
    pub(super) fn ensure_stream_epoch(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        now_ms: u64,
    ) -> rusqlite::Result<(u32, StreamKey)> {
        if let Some(found) = self.keychain.current_stream_key_tx(tx, stream_id)? {
            return Ok(found);
        }
        let (epoch, key) = self
            .keychain
            .mint_epoch(tx, stream_id, self.rng.as_ref(), now_ms)?;
        self.emit_key_envelopes(tx, stream_id, epoch, &key, now_ms, None)?;
        Ok((epoch, key))
    }

    /// Emit one `key_envelope` op per recipient for `(stream_id, epoch)`.
    ///
    /// Recipients are every **unrevoked** device other than this one — which
    /// already holds the key — plus the account identity, whose copy is what
    /// makes a recovery with no surviving device restore readable content
    /// rather than an empty vault.
    ///
    /// The exclusion of revoked devices is load-bearing here in a way it was
    /// not when `#76` was filed. It was vacuous then because the identity copy
    /// was sealed to `ID_D_pub` while pairing handed every device `ID_D_priv`:
    /// a device dropped from the recipient list opened the identity copy
    /// instead and lost nothing. `ID_D_priv` no longer travels in a
    /// `PairingPayload` — it exists only inside the recovery blob, behind the
    /// BIP-39 code — so the identity copy is now openable by the recovery-code
    /// holder alone, and dropping a device from this list is the whole of what
    /// stops it reading the epoch.
    ///
    /// The consequence for an unrevoked device is that this list is now the
    /// *only* way it learns an epoch minted after it paired, which is why
    /// [`Self::backfill_key_envelopes`] exists: a device certified after an
    /// epoch was minted would otherwise never receive it.
    ///
    /// `seal_under` chooses the epoch these ops are themselves sealed at; see
    /// [`Self::ops_insert_at`]. `None` means "whatever the meta stream's live
    /// epoch is", which is right for a first mint and wrong for a rotation.
    pub(super) fn emit_key_envelopes(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        epoch: u32,
        key: &StreamKey,
        now_ms: u64,
        seal_under: Option<&(u32, StreamKey)>,
    ) -> rusqlite::Result<()> {
        let key_id = stream_key_id(key);
        let mut recipients: Vec<(Recipient, [u8; 32])> = Vec::new();
        {
            let mut stmt = tx.prepare(
                // The anti-join against `device_revocations` is the read half
                // of revocation. A device with a recorded cut gets no envelope
                // for any epoch minted at or after it, and since `ID_D_priv`
                // stopped travelling in a `PairingPayload` there is no second
                // copy for it to open instead.
                //
                // The presence of the row is the whole test -- there is no
                // `cut_ms` comparison, and [`Self::is_revoked`] explains at
                // length why a correct one is indistinguishable from this and
                // an incorrect one silently collapses to a wall clock after a
                // restart.
                //
                // This is also what the revoking transaction relies on for the
                // device it is revoking: `revoke_device` writes the register
                // before it mints anything, so by the time any seal in that
                // transaction reaches this query the row is already here.
                //
                // What no test here closes: the register is per-replica, so a
                // device that has not yet applied the `device_revoke` op has no
                // row to read and will seal this epoch to the revoked device.
                // Revocation propagates like every other op.
                "SELECT d.device_id, d.d_d_pub FROM devices d
                 WHERE d.d_d_pub IS NOT NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM device_revocations r
                       WHERE r.device_id = d.device_id
                   )",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (id, pubkey) in rows {
                let Some(id) = to16(&id) else {
                    continue;
                };
                if id == self.keychain.device_id() {
                    continue;
                }
                if pubkey.len() != 32 {
                    continue;
                }
                let mut pk = [0u8; 32];
                pk.copy_from_slice(&pubkey);
                recipients.push((Recipient::Device(id), pk));
            }
        }
        recipients.push((
            Recipient::Identity(self.keychain.identity_id()),
            self.keychain.identity_dh_pub(),
        ));

        for (recipient, recipient_pub) in recipients {
            let Ok(hpke_ciphertext) = self.keychain.seal_key_envelope(
                &recipient_pub,
                stream_id,
                epoch,
                key,
                self.rng.as_ref(),
            ) else {
                // A device whose stored `d_d_pub` is not a usable X25519 point
                // is skipped rather than failing the whole command: one corrupt
                // row must not make the vault unwritable.
                continue;
            };
            let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
                stream_id: *stream_id,
                epoch,
                recipient,
                key_id,
                hpke_ciphertext,
            });
            self.emit_control_op(tx, &inner, now_ms, seal_under)?;
            if let Recipient::Device(id) = recipient {
                record_envelope_recipient(tx, stream_id, epoch, &id, now_ms)?;
            }
        }
        Ok(())
    }

    /// Emit the `key_envelope` ops `device_id` is missing, and no others.
    ///
    /// Called when a `device_cert` is applied. Before `#76` this did not need
    /// to exist: a device left out of an epoch's recipient list opened the
    /// identity copy instead, so the gap between "an epoch was minted" and
    /// "the minter had heard of this device" closed itself. It does not close
    /// itself any more, and the gap is not rare — creating a Stream mints its
    /// first epoch, so a Stream created on one device while a second device's
    /// cert was still in flight would be unreadable on that second device
    /// permanently.
    ///
    /// Every device that applies the cert runs this, and that is deliberate:
    /// picking one emitter would mean picking a device that might be offline.
    /// `key_envelope_recipients` carries the first emitter's rows along with
    /// its ops, so a device that applies those before it applies the cert
    /// emits nothing. The first round is still a race between however many
    /// devices are online, and absorption being idempotent is what makes that
    /// merely wasteful.
    ///
    /// The set is **every** epoch this replica holds, not the current one per
    /// stream. Current-epoch-only was the first shape and it was wrong for a
    /// reason that is ordinary rather than adversarial: a rotation landing
    /// between a device's pairing and its certificate leaves that device
    /// holding the payload's epochs and the live one, with a permanent hole in
    /// between. See [`Keychain::held_epochs_tx`] for the cost this trades
    /// against and for the derivation that was rejected.
    ///
    /// A revoked device is skipped, on the same presence test the recipient
    /// query uses, so this route cannot readmit a device the rotation just
    /// excluded. Without it, a revocation followed by the revoked device
    /// republishing its own cert would hand back everything the revocation had
    /// just rotated away. Without that, a revocation followed by the revoked
    /// device republishing its own cert would hand back everything the
    /// revocation had just rotated away.
    pub(super) fn backfill_key_envelopes(
        &self,
        tx: &Transaction<'_>,
        device_id: &[u8; 16],
        device_pub: &[u8; 32],
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        if *device_id == self.keychain.device_id() {
            return Ok(());
        }
        if self.is_revoked(tx, device_id)? {
            return Ok(());
        }
        for (stream_id, epoch) in Keychain::held_epochs_tx(tx)? {
            let already: i64 = tx.query_row(
                "SELECT count(*) FROM key_envelope_recipients
                 WHERE stream_id = ? AND epoch = ? AND recipient = ?",
                params![&stream_id[..], epoch, &device_id[..]],
                |r| r.get(0),
            )?;
            if already > 0 {
                continue;
            }
            let keys = self.keychain.stream_keys_at(&stream_id, epoch);
            let Some(key) = keys.first() else {
                continue;
            };
            let Ok(hpke_ciphertext) = self.keychain.seal_key_envelope(
                device_pub,
                &stream_id,
                epoch,
                key,
                self.rng.as_ref(),
            ) else {
                continue;
            };
            let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
                stream_id,
                epoch,
                recipient: Recipient::Device(*device_id),
                key_id: stream_key_id(key),
                hpke_ciphertext,
            });
            self.emit_control_op(tx, &inner, now_ms, None)?;
            record_envelope_recipient(tx, &stream_id, epoch, device_id, now_ms)?;
        }
        Ok(())
    }

    /// Log one control op into the vault-meta stream.
    ///
    /// The epoch is resolved **before** the sequence number is read, and that
    /// order is load-bearing. Resolving it can mint the vault-meta stream's own
    /// first key, which emits `key_envelope` ops into this same stream; a `seq`
    /// read before that happened would already be taken by the time this op
    /// reached the log, and `ops` has a `UNIQUE(stream_id, device_id, seq)`.
    /// The op would be ignored, and its outbox row would then fail its foreign
    /// key — which is exactly how this was found.
    pub(super) fn emit_control_op(
        &self,
        tx: &Transaction<'_>,
        inner: &InnerOp,
        now_ms: u64,
        seal_under: Option<&(u32, StreamKey)>,
    ) -> rusqlite::Result<()> {
        let blob = encode_inner_op(inner).map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
        let (epoch, key) = match seal_under {
            Some((epoch, key)) => (*epoch, key.clone()),
            None => self.ensure_stream_epoch(tx, &META_STREAM, now_ms)?,
        };
        let op_id = self.fresh_op_id(now_ms);
        let seq = self.next_seq_tx(tx, &META_STREAM)?;
        let hlc = self.hlc.send();
        self.ops_insert_at(
            tx,
            &op_id,
            &META_STREAM,
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
        )
    }

    /// Next `seq` for `(stream_id, this device)`, read from the committed DB.
    pub(super) fn next_seq(&self, db: &Db, stream_id: &[u8; 16]) -> Result<u64, EngineError> {
        let stream_blob: Vec<u8> = stream_id.to_vec();
        let device_blob: Vec<u8> = self.keychain.device_id().to_vec();
        let max: Option<i64> = db
            .conn()
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM ops WHERE stream_id = ? AND device_id = ?",
                params![stream_blob, device_blob],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let next = max.map_or(1, |v| v.saturating_add(1));
        Ok(u64::try_from(next).unwrap_or(1))
    }

    /// Next `seq` for `(stream_id, this device)`, read inside a transaction so it
    /// sees uncommitted inserts from earlier in the same transaction.
    pub(super) fn next_seq_tx(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
    ) -> rusqlite::Result<u64> {
        let stream_blob: Vec<u8> = stream_id.to_vec();
        let device_blob: Vec<u8> = self.keychain.device_id().to_vec();
        let max: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM ops WHERE stream_id = ? AND device_id = ?",
            params![stream_blob, device_blob],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(max.saturating_add(1)).unwrap_or(1))
    }

    /// Decode + verify + open the stored envelope for `op_id` back to its
    /// inner-op CBOR, using the keychain. Proves the full seal/unseal cycle;
    /// used by the engine's own tests.
    #[cfg(test)]
    pub(crate) fn open_op_row(&self, db: &Db, op_id: &[u8; 16]) -> Result<Vec<u8>, EngineError> {
        let env = OpLog::get_envelope(db, op_id)?
            .ok_or_else(|| EngineError::NotFound(format!("op {}", hex_short(op_id))))?;
        self.keychain
            .open_op(&env)
            .map_err(|e| EngineError::Invalid(e.to_string()))
    }
}

/// Deterministic op-id for a received op, derived from
/// `(stream_id, device_id, seq)`. Two replicas that receive the same op assign
/// it the same op-log primary key, reinforcing the `UNIQUE(stream, device, seq)`
/// idempotence gate.
pub(super) fn remote_op_id(stream_id: &[u8; 16], device_id: &[u8; 16], seq: u64) -> [u8; 16] {
    let mut km = Vec::with_capacity(16 + 16 + 8);
    km.extend_from_slice(stream_id);
    km.extend_from_slice(device_id);
    km.extend_from_slice(&seq.to_be_bytes());
    let bytes = sunrise_crypto::derive_key("sunrise.remote_op_id.v1", &km, 16);
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    out
}

/// The end of the run of `ops` seqs starting at `start`, or `start - 1` when
/// `start` itself is absent.
/// Note that `recipient` has been sent the key for `(stream_id, epoch)`.
///
/// `INSERT OR IGNORE`: two devices can back-fill the same recipient
/// concurrently, and both will record it. The row is a *hint* — being absent
/// costs a redundant envelope, and being present when the envelope never
/// arrived is the one failure that matters, which is why it is only ever
/// written alongside an op that carries the key, never on its own.
pub(super) fn record_envelope_recipient(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    epoch: u32,
    recipient: &[u8; 16],
    now_ms: u64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO key_envelope_recipients
         (stream_id, epoch, recipient, recorded_at_ms) VALUES (?, ?, ?, ?)",
        params![&stream_id[..], epoch, &recipient[..], now_ms],
    )?;
    Ok(())
}

fn ops_run_end(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
    start: i64,
) -> rusqlite::Result<i64> {
    tx.query_row(
        "SELECT CASE
                  WHEN EXISTS (SELECT 1 FROM ops
                               WHERE stream_id = ?1 AND device_id = ?2 AND seq = ?3)
                  THEN (SELECT MIN(o.seq) FROM ops o
                        WHERE o.stream_id = ?1 AND o.device_id = ?2 AND o.seq >= ?3
                          AND NOT EXISTS (SELECT 1 FROM ops n
                                          WHERE n.stream_id = ?1 AND n.device_id = ?2
                                            AND n.seq = o.seq + 1))
                  ELSE ?3 - 1
                END",
        params![&stream_id[..], &device_id[..], start],
        |row| row.get(0),
    )
}

/// Set `sync_cursors(stream_id, device_id)` to the end of the **contiguous**
/// applied prefix — the largest `n` for which every seq `1..=n` from that
/// device on that stream is in the op log.
///
/// A refused op is *not* decided and does not appear here. It was, briefly: an
/// op refused for a revoked sender advanced this past it so the relay would
/// stop resending. That made a reversible decision irreversible — the cut can
/// rise as well as fall — so a refusal now leaves the cursor where it is and
/// the op applies if it is resent under a corrected cut.
///
/// A high-water mark would be wrong, and used to be what this wrote. Ops do
/// arrive out of order: a dropped frame followed by a later one leaves the log
/// holding seqs `{1, 3}`. A `MAX` cursor then claims 3, and the relay — which
/// now filters its replay by exactly this number (issue #19) — would skip the
/// frame carrying seq 2 forever. The hole would never be refilled and never be
/// noticed. That is silent data loss produced by the very mechanism meant to
/// prevent it, so the cursor has to mean "I have everything through n", which
/// is also how `CursorEntry.last_applied_seq` is read on the wire.
pub(super) fn upsert_sync_cursor(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
) -> rusqlite::Result<()> {
    let prefix = ops_run_end(tx, stream_id, device_id, 1)?;
    tx.execute(
        "INSERT INTO sync_cursors (stream_id, device_id, last_applied_seq)
         VALUES (?, ?, ?)
         ON CONFLICT(stream_id, device_id) DO UPDATE SET
            last_applied_seq = MAX(last_applied_seq, excluded.last_applied_seq)",
        params![&stream_id[..], &device_id[..], prefix],
    )?;
    Ok(())
}
