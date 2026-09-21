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
use super::lww::LwwStamp;
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
        let head = self.current_identity(tx)?;
        let mut recipients: Vec<(Recipient, [u8; 32])> = Vec::new();
        {
            let mut stmt = tx.prepare(
                // The anti-join against `device_read_bounds` is the read half
                // of revocation. A device the bound names gets no envelope for
                // any epoch minted afterwards, and since `ID_D_priv` stopped
                // travelling in a `PairingPayload` there is no second copy for
                // it to open instead.
                //
                // The presence of the row is the whole test -- there is no
                // `cut_ms` comparison, and [`Self::is_revoked`] explains at
                // length why a correct one is indistinguishable from this and
                // an incorrect one silently collapses to a wall clock after a
                // restart.
                //
                // This is also what the revoking transaction relies on for the
                // device it is revoking: `revoke_device` applies the op --
                // which folds the register and takes the bound -- before it
                // mints anything, so by the time any seal in that transaction
                // reaches this query the row is already here.
                //
                // What no test here closes: the bound is per-replica, so a
                // device that has not yet applied the `device_revoke` op has no
                // row to read and will seal this epoch to the revoked device.
                // Revocation propagates like every other op.
                //
                // The `identity_id` clause is the other half, and it does what
                // the anti-join structurally cannot. A revoked device that
                // mints a fresh id has no revocation row to be excluded by, so
                // the anti-join lets the new name through forever. What it does
                // not have is a cert under the identity *in force*: the
                // rotation that accompanies the revocation moves the head, and
                // the revoked device cannot follow, because the successor's
                // `ID_S_priv` travelled as HPKE shares sealed to the surviving
                // devices' `D_D_pub` and it is not one of them.
                //
                // This clause bounds every *subsequent* epoch, which is the
                // failure ADR-0032's alternative 3 could not close: a one-shot
                // check at admission time leaves the device on the recipient
                // list for everything minted afterwards.
                //
                // The anti-join is against `device_read_bounds` and **not**
                // against the register. Since ADR-0041 the register is derived
                // and can take a row back out — a revocation stops being
                // believed when the ledger shows its author was revoked first
                // — and a recipient gate that a later op can release is not a
                // gate. `device_read_bounds` only ever grows
                // (`migrations/0028_device_read_bounds.sql`), so a device this
                // replica once excluded stays excluded from every epoch it
                // mints afterwards, whatever the device list goes on to say
                // about it.
                "SELECT d.device_id, d.d_d_pub FROM devices d
                 WHERE d.d_d_pub IS NOT NULL
                   AND d.identity_id = ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM device_read_bounds b
                       WHERE b.device_id = d.device_id
                   )",
            )?;
            let rows = stmt
                .query_map(params![&head.identity_id[..]], |r| {
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
    /// A **read-bounded** device is skipped, on the same presence test the
    /// recipient query uses, so this route cannot readmit a device the rotation
    /// just excluded. Without it, a revocation followed by the revoked device
    /// republishing its own cert would hand back everything the revocation had
    /// just rotated away.
    ///
    /// The test is [`Self::is_read_bounded`] and not [`Self::is_revoked`], and
    /// this is the site where the difference costs the most. The register is
    /// derived, so a revocation stops being believed once the ledger shows its
    /// author was revoked first; against the register this early return then
    /// stopped firing, and a single `DeviceCertPublish` from the unwound device
    /// pulled **every held epoch of every stream** back out — not merely the
    /// epochs minted from then on. `device_read_bounds` never gives a row back,
    /// so the hand-back stays closed however the device list changes.
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
        if self.is_read_bounded(tx, device_id)? {
            return Ok(());
        }
        // **This is the line that closes #105's round trip.**
        //
        // Step 4 of the issue is "`backfill_key_envelopes` then seals C' the
        // current epoch of every stream": a device revoked under one id mints a
        // fresh one, self-signs a cert with the `ID_S_priv` it kept, publishes
        // it, and this function hands the new name every key the revocation had
        // just rotated away. The revocation register cannot stop it — the new
        // id is not in it and never was — and no check on the cert can either,
        // because the cert is genuine.
        //
        // What stops it is that the cert is genuine under the *wrong* identity.
        // A rotation accompanies the revocation, so the head has moved; the
        // `DeviceCertPublish` arm recorded which chain identity verified the
        // cert; and a device whose row names anything but the head is not a
        // member. Nothing here is a judgement about the device — it is a
        // comparison of two stored values, so every replica holding the same op
        // set reaches the same answer.
        let head = self.current_identity(tx)?;
        let member: Option<Vec<u8>> = tx
            .query_row(
                "SELECT identity_id FROM devices WHERE device_id = ?",
                params![&device_id[..]],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if member.as_deref().and_then(to16) != Some(head.identity_id) {
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

    /// Everything a vault-meta write needs, taken inside `tx` and in the one
    /// order that is safe.
    ///
    /// The order is the whole value of this function, and it is not obvious
    /// enough to be left to eleven call sites to remember. Resolving the epoch
    /// can **mint** the vault-meta stream's own first key, and minting emits a
    /// `key_envelope` op per recipient into that same stream — so a sequence
    /// number taken before the epoch is already spent by the time the caller's
    /// op reaches the log. `ops` has a `UNIQUE(stream_id, device_id, seq)` and
    /// [`OpLog::insert`] is `INSERT OR IGNORE`, so the losing op vanishes
    /// without a word and its outbox row then fails its foreign key.
    ///
    /// That defect was fixed once for `emit_control_op` (`60ee61d`) and once
    /// for `create_stream` (`#214`) before it was clear that the right fix was
    /// to make it unstatable. There is now no way to obtain a vault-meta `seq`
    /// outside a transaction that has already resolved the epoch, because this
    /// is the only thing that hands one out.
    ///
    /// The [`LwwStamp`] comes with it because [`Engine::lww_stamp`] must be
    /// called **once** per emitted op and the `seq` it carries is this one;
    /// handing back a `seq` without its stamp invites a caller to stamp with a
    /// second [`HlcClock::send`](crate::config::HlcClock::send).
    pub(super) fn meta_slot(
        &self,
        tx: &Transaction<'_>,
        now_ms: u64,
    ) -> rusqlite::Result<MetaSlot> {
        let (epoch, key) = self.ensure_stream_epoch(tx, &META_STREAM, now_ms)?;
        let seq = self.next_seq_tx(tx, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        Ok(MetaSlot {
            epoch,
            key,
            seq,
            lww,
        })
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

/// One vault-meta log slot: the epoch to seal under, the key, the sequence
/// number, and the LWW stamp that goes on whatever row the op materializes.
///
/// Produced only by [`Engine::meta_slot`], which is what makes the order the
/// four are taken in unstatable rather than merely documented.
pub(super) struct MetaSlot {
    /// Live epoch of the vault-meta stream, resolved (and possibly minted)
    /// before `seq` was read.
    pub(super) epoch: u32,
    /// The key for `epoch`.
    pub(super) key: StreamKey,
    /// Next `seq` for `(vault-meta, this device)`, read after any mint.
    pub(super) seq: u64,
    /// The stamp for the row this op writes. Carries `seq`.
    pub(super) lww: LwwStamp,
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

/// Record that `recipient` has been sent the key for `(stream_id, epoch)`.
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

/// The end of the run of `ops` seqs for `(stream_id, device_id)` starting at
/// `start`, or `start - 1` when `start` itself is absent.
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
/// stop resending. That is history, and what replaced it is not a narrower
/// refusal of the op but a refusal of something else. **Nothing below the
/// idempotence gate declines the op itself**: once the op row is in, no refusal
/// removes it, so the cursor counts the op whatever answer it got. Refusals
/// above it return before the transaction opens and leave no op row and no
/// cursor, the half pinned by
/// `crates/sunrise-core/src/engine/tests.rs:8427#a_stranger_cert_through_apply_remote_is_refused_and_writes_no_cursor`.
///
/// That is a weaker claim than "the apply path does not consult revocation",
/// and deliberately so, because the apply path does consult it — in three
/// places, and the enumeration of record is
/// `docs/01-architecture/threat-model.md:73` rather than this comment, which
/// would otherwise be a fourth copy of a list that has already drifted once.
/// What each consultation settles is key distribution or a register row. The
/// enumeration below names the ones a reader of *this* function has to know
/// about, with exactly what each leaves behind.
///
/// The argument that removed the old behaviour — that advancing made a
/// reversible decision irreversible — is now settled rather than hedged, and
/// settled in its favour. The register really is reversible:
/// `crates/sunrise-core/src/engine/revocation.rs:1225#refold_device_revocations`
/// empties `device_revocations` and rebuilds it on every applied revocation,
/// and what it rebuilds is last-writer-wins rather than `MIN` — the ledger is
/// folded in ascending order at
/// `crates/sunrise-core/src/engine/revocation.rs:1056#refold_device_revocations`
/// so a later row simply overwrites an earlier one, written at
/// `crates/sunrise-core/src/engine/revocation.rs:1117#refold_device_revocations`
/// — precisely so a cut from a slow clock is corrected by revoking again from
/// a healthy device. The rationale for choosing LWW over `MIN` is recorded at
/// `crates/sunrise-core/src/engine/revocation.rs:1269#apply_device_revoke`.
/// Nothing below rests on that, because nothing below un-writes an op row.
///
/// # What the apply path consults, and what that read decides
///
/// The read bound, and not the revocation register. A `DeviceCertPublish`
/// dispatched out of `crates/sunrise-core/src/engine/sync.rs:312#apply_remote_all`
/// reaches `crates/sunrise-core/src/engine/oplog.rs:418#backfill_key_envelopes`,
/// which returns early on a device
/// `crates/sunrise-core/src/engine/revocation.rs:636#is_read_bounded` names —
/// a presence test over `device_read_bounds`, inside the apply transaction, on
/// remote input. [`Engine::is_revoked`] is consulted nowhere on this path: its
/// one non-test caller is the local command at
/// `crates/sunrise-core/src/engine/revocation.rs:257#revoke_device`. ADR-0041
/// (`docs/11-adr/0041-peer-side-revocation-is-a-fold.md`) is where the two
/// tables were split, and the split is why the distinction earns a sentence:
/// the register is a fold and shrinks, `device_read_bounds` only ever grows,
/// and the site that hands out keys must not be able to hand them back.
///
/// What that read decides is which stream keys a newly certified device is
/// sealed: on a bounded one it returns early and seals none. What it does
/// *not* decide is whether the op applies. The op row went in at the
/// idempotence gate before the control op was dispatched, the `devices` row is
/// written before the backfill is attempted, and a backfill error is logged rather than
/// raised (`crates/sunrise-core/src/engine/sync.rs:968#apply_control_op`). Not one of
/// those answers skips this function: it runs on the delivery like any other,
/// and the op row it left behind counts toward the prefix like any other.
/// Whether the number this writes actually moves is a question about the seqs
/// *below* that op and never about the answer the op got — see the
/// out-of-order paragraph at the end.
/// `crates/sunrise-core/src/engine/tests.rs:7898#a_revoked_device_cert_through_apply_remote_seals_no_keys`
/// walks that trace from the envelope down to the early return, and
/// `crates/sunrise-core/src/engine/tests.rs:8158#a_failed_backfill_is_logged_and_the_cert_delivery_still_applies`
/// holds the backfill-error answer: the error is logged, the cert stands, and
/// this still runs.
///
/// Admission is settled at step b — by the `devices` lookup at
/// `crates/sunrise-core/src/engine/sync.rs:211#apply_remote_all`, or, for the
/// `DeviceCertPublish` family that trace delivers, by
/// [`Engine::self_authenticating_signer`] at
/// `crates/sunrise-core/src/engine/sync.rs:217#apply_remote_all`, which checks
/// the envelope against the cert it carries because that op is what *creates*
/// the row the lookup reads. In neither case is it settled by the register or
/// by the bound: a revoked device's row is found there like any other and its
/// op is applied like any other. ADR-0034
/// (`docs/11-adr/0034-revocation-bounds-reads-not-writes.md`) is where that
/// was decided — revocation bounds what a device may *read*, not whether what
/// it writes lands.
/// `crates/sunrise-core/src/engine/tests.rs:7495#a_revoked_devices_ops_still_apply_at_the_replica`
/// holds the admitting half, and
/// `crates/sunrise-core/src/engine/tests.rs:8427#a_stranger_cert_through_apply_remote_is_refused_and_writes_no_cursor`
/// the refusing one, which never reaches this function at all.
///
/// # Where this sits relative to the op row
///
/// This function decides nothing and refuses nothing: it writes the end of the
/// run already in the log, and both of its call sites reach it having put the
/// op row in first — `crates/sunrise-core/src/engine/sync.rs:318#apply_remote_all`
/// at its step g, past an idempotence gate that returns early when the insert
/// changed no row, and
/// `crates/sunrise-core/src/engine/oplog.rs:153#ops_insert_at` at the tail of
/// this device's own emit, after the op-log insert and the outbox enqueue.
///
/// A local emit that fails never reaches this. Three statements before it can
/// fail — the seal at
/// `crates/sunrise-core/src/engine/oplog.rs:123#ops_insert_at`, the
/// `OpLog::insert` error arm at
/// `crates/sunrise-core/src/engine/oplog.rs:124#ops_insert_at`, and the
/// `Outbox::enqueue` at
/// `crates/sunrise-core/src/engine/oplog.rs:146#ops_insert_at` — and
/// `crates/sunrise-core/src/engine/tests.rs:8294#a_failed_local_emit_leaves_the_cursor_where_it_was`
/// walks the second and the third, inside the transaction rather than after
/// the rollback. The seal is the one it does not walk. Nor could any of them
/// move the number if it did: the prefix is read out of `ops`, never off the
/// op being written, so a row that is not in the log cannot be counted by it.
///
/// # The refusals that do survive
///
/// Two of them belong to revocation, and neither is an op's.
///
/// The first. `crates/sunrise-core/src/engine/sync.rs:767#apply_control_op` hands a
/// `device_revoke` to
/// `crates/sunrise-core/src/engine/revocation.rs:1293#apply_device_revoke`,
/// which refuses one naming its own sender — logging
/// `core.device.revoke_refused` with `reason = "self"` — and writes no
/// register row. That event has a second emitter at
/// `crates/sunrise-core/src/engine/revocation.rs:1352#apply_device_revoke`,
/// `reason = "revoked_sender"`, and that one is not a refusal to record at
/// all: the ledger row stands and the fold declines to believe it. A reader
/// who wants every emitter of `revoke_refused` has both of them here.
///
/// What is refused in the first case is a *register write* rather than the
/// delivery: the op row went in before the control op was dispatched, so this
/// still runs afterwards and counts that op toward the prefix like any other.
/// A reader arrives here expecting the opposite, which is why it is named, and
/// why
/// `crates/sunrise-core/src/engine/tests.rs:7584#a_self_refused_revoke_still_advances_the_cursor`
/// holds both halves — for a delivery at seq 1, where the contiguous prefix is
/// that op alone — and observes the log line rather than assuming it.
///
/// The second, and it is the one keyed on the *sender's* standing that the
/// premise at the top of this comment turns on.
/// `crates/sunrise-core/src/engine/sync.rs:705#apply_control_op` refuses a
/// read-bounded sender's claim that some third device already holds the key at
/// a `(stream, epoch)`, logging `core.key.recipient_claim_refused`. What it
/// declines is a `key_envelope_recipients` **hint** row and nothing else: the
/// op row went in at the idempotence gate before the control op was
/// dispatched, this function runs afterwards at step g, and the cursor counts
/// that op exactly like an applied one. The recipients table is a hint rather
/// than state, so a replica that declines the row finds no row and
/// `backfill_key_envelopes` emits the envelope anyway — the gate can only ever
/// cause *more* key distribution, never less, which is why it reads
/// `crates/sunrise-core/src/engine/revocation.rs:636#is_read_bounded` and
/// tolerates that predicate being per-replica
/// ([#282](https://github.com/justin13888/Sunrise/issues/282)).
/// `crates/sunrise-core/src/engine/tests.rs:7755#a_read_bounded_senders_recipient_claim_is_refused_and_the_cursor_counts_the_op`
/// delivers one through `apply_remote_all` — the only route that can see this
/// half at all — and holds all three: the declined hint row, the op row, and
/// the cursor.
///
/// Two further revocation refusals live on the local command path and neither
/// reaches an op or this function: [`Engine::revoke_device`] refuses a
/// self-revocation, and refuses a target with no `devices` row.
///
/// **Refusals outside revocation are a different count, and this paragraph
/// does not bound them.** At least three live on the apply path above:
/// `crates/sunrise-core/src/engine/sync.rs:244#apply_remote_all` when no key
/// at this `(stream, epoch)` opens the envelope,
/// `crates/sunrise-core/src/engine/sync.rs:259#apply_remote_all` when the
/// sender's clock is outside the drift window, and
/// `crates/sunrise-core/src/engine/sync.rs:613#apply_control_op` when a key
/// envelope names an epoch above `MAX_EPOCH_LEAP`. The first two return an
/// error before the transaction opens, so no op row and no cursor; the third
/// drops a payload with the op row already in, so the op counts toward the
/// prefix exactly like an applied one.
///
/// Nor would resending recover anything **for an op that reached the op log**:
/// its id is derived from `(stream_id, device_id, seq)` by [`remote_op_id`], so
/// a resent op carries the op log's same primary key, collides on insert, and
/// [`Engine::apply_remote_all`] returns at its idempotence gate without
/// re-running materialization or [`Engine::apply_control_op`] at all. That is
/// `crates/sunrise-core/src/engine/tests.rs:5960#apply_remote_is_idempotent`
/// for an entity op, and the tail of
/// `crates/sunrise-core/src/engine/tests.rs:7584#a_self_refused_revoke_still_advances_the_cursor`
/// for a control one, which resends *different* payload bytes under the same
/// `(stream, device, seq)` and observes that the control arm never sees them.
///
/// # Across a re-fold
///
/// The register is derived and non-monotone, so a delivery can retract a
/// decision this replica had already made: applying one `device_revoke` can
/// unwind another, and `core.device.revocation_unwound` is a routine event.
/// **Nothing the fold does reaches this number.** The fold rewrites
/// `device_revocations`; the cursor is a fact about `ops`, and the op whose
/// effect was unwound is still in the log and still counted. This function never
/// lowers it — the `ON CONFLICT` arm below takes a `MAX` — but it does not even
/// try to. The delivery that folded runs this once, for
/// its own `(stream, device)` and at its step g like any other delivery; what
/// no call site triggers is an *extra* run for the devices whose revocations
/// that fold unwound, and none is needed, because their cursors are facts
/// about `ops` that the fold did not touch.
/// `crates/sunrise-core/src/engine/tests.rs:8536#a_refold_that_unwinds_a_revocation_moves_no_cursor`
/// pins all of it, including the half that does *not* unwind:
/// `device_read_bounds` keeps the device it bounded, which is the asymmetry
/// `crates/sunrise-core/src/engine/revocation.rs:636#is_read_bounded` exists
/// for.
///
/// A high-water mark would be wrong, and used to be what this wrote. Ops do
/// arrive out of order: a dropped frame followed by a later one leaves the log
/// holding seqs `{1, 3}`. A `MAX` cursor then claims 3, and the relay — which
/// now filters its replay by exactly this number (issue #19) — would skip the
/// frame carrying seq 2 forever. The hole would never be refilled and never be
/// noticed. That is silent data loss produced by the very mechanism meant to
/// prevent it, so the cursor has to mean "I have everything through n", which
/// is also how `CursorEntry.last_applied_seq` is read on the wire.
///
/// That meaning is what bounds every "the cursor advances" above. The run
/// starts at seq 1 because this function asks for it there —
/// `crates/sunrise-core/src/engine/oplog.rs:921#upsert_sync_cursor` is where
/// the literal lives;
/// `crates/sunrise-core/src/engine/oplog.rs:670#ops_run_end` is parameterised
/// on `start` at
/// `crates/sunrise-core/src/engine/oplog.rs:674#ops_run_end` and hard-codes
/// nothing. So an op delivered with a gap below it is in the log and outside
/// the prefix: with the log holding `{2}` the `ELSE ?3 - 1` arm writes 0, and with
/// it holding `{1, 3}` the run ends at 1. Refused or applied makes no
/// difference to either, which is what
/// `crates/sunrise-core/src/engine/tests.rs:8039#a_self_refused_revoke_out_of_order_leaves_the_cursor_short`
/// pins.
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

#[cfg(test)]
mod frozen_domain {
    use sunrise_crypto_test_vectors::at_rest::REMOTE_OP_ID_VECTORS;

    /// `sunrise.remote_op_id.v1`, anchored to frozen literals.
    ///
    /// Two replicas have to assign a received op the same op-log primary key
    /// without ever exchanging it — that agreement is what makes
    /// `UNIQUE(stream, device, seq)` idempotent *across* devices rather than
    /// only within one. The derivation is never transmitted, so a build that
    /// spelled the context differently, or ordered the three inputs
    /// differently, would store every peer's op under an id no peer agrees
    /// with and notice nothing.
    ///
    /// The expectations are literals in `sunrise-crypto-test-vectors`, which
    /// depends on nothing, so a rename here cannot be absorbed by editing the
    /// test.
    #[test]
    fn remote_op_id_vectors_hold() {
        for v in REMOTE_OP_ID_VECTORS {
            assert_eq!(
                super::remote_op_id(&v.stream_id, &v.device_id, v.seq),
                v.op_id,
                "the sunrise.remote_op_id.v1 derivation drifted at seq {}",
                v.seq
            );
        }
    }
}
