//! Device lifecycle and the receive half of sync.
//!
//! Everything a *remote* op or a device-membership change touches: the
//! `devices`, `device_revocations`, `deferred_ops` and `key_envelope` tables,
//! the [`crate::control_op`] families, and the verify/open surface of
//! `sunrise_crypto`. Nothing here reads a Task.
//!
//! The asymmetry with the command path is the point of the boundary. A command
//! validates a draft this device just authored; this path validates a signature
//! from a peer it may no longer trust, and has to decide — in that order —
//! whether the sender is revoked, whether the key that opens the op has arrived
//! yet, and only then what the op says.

use super::ids::hex_short;
use super::lww::{materialize_remote, remap_legacy_inbox, LwwStamp};
use super::oplog::{record_envelope_recipient, remote_op_id, upsert_sync_cursor};
use super::{
    Engine, EngineError, DEFERRED_PER_EPOCH_CAP, DEFERRED_TOTAL_CAP, DEFERRED_TTL_MS,
    MAX_EPOCH_LEAP, META_STREAM,
};
use crate::commands::CommandResult;
use crate::control_op::{DeviceRevokePayload, Recipient, RevokeReason};
use crate::events::DomainEvent;
use crate::inner_op::{decode_inner_op, encode_inner_op, InnerOp, OpEffect};
use crate::keychain::{EnvelopeRecipient, KeySource};
use rusqlite::{params, OptionalExtension, Transaction};
use sunrise_cbor::hlc::Hlc;
use sunrise_crypto::op_envelope::open_envelope;
use sunrise_crypto::{
    decode_envelope, open_envelope_unverified, stream_key_id, verify_envelope, DeviceCert,
};
use sunrise_id::EntityRef;
use sunrise_storage::{Db, OpLog};

impl Engine {
    /// Record a device as revoked, and mint a new epoch for every stream it
    /// could read.
    ///
    /// **This bounds the revoked device's reads, and nothing else.** Nothing
    /// queues anything, and nothing bounds its writes — see
    /// [`Self::apply_remote`] step 2 for why the relay cannot be told and why
    /// peers do not refuse. It records a cut every replica converges on, rotates
    /// every stream in the rotation set, and seals the new epochs to everyone
    /// *except* the device it just revoked — which is only meaningful because
    /// `PairingPayload` no longer carries `ID_D_priv`, so there is no
    /// identity-sealed copy for that device to open instead.
    ///
    /// It cannot bound what the device already had; no rotation can. And it
    /// does not bound the account's *creator*, which keeps `ID_D_priv` until a
    /// recovery blob exists to hold it — self-revocation is refused below, so
    /// reaching that case means revoking the creator from another device.
    ///
    /// Four things happen in one transaction, and the order is the design:
    ///
    /// 0. The revocation register is written **first**, before anything can
    ///    mint. `ensure_stream_epoch` below emits a `key_envelope` per
    ///    recipient, and with an empty register the device this transaction
    ///    exists to revoke would be one of them.
    /// 1. The `DeviceRevoke` op is emitted, and the register write above goes
    ///    through the same [`Self::apply_control_op`] a remote op takes, so
    ///    the local and remote paths cannot disagree.
    /// 2. Every stream in the rotation set — the vault-meta stream and the
    ///    Inbox included, not only user Streams — mints a fresh epoch.
    /// 3. The `key_envelope` ops carrying those keys are sealed under the
    ///    **pre-rotation** vault-meta epoch, because a device that has not yet
    ///    received the new meta key cannot read an op sealed under it.
    ///
    /// The vault-meta stream is in the rotation set so that the *shape* of the
    /// account — its Streams, Contexts and Routines — rotates with its contents
    /// rather than staying on one key forever, which is why a revoked device
    /// stops seeing new Streams and Contexts and not merely new tasks.
    ///
    /// What no rotation could ever do: the revoked device keeps every key it
    /// already held, so it keeps everything it could already read. Rotation
    /// bounds forward exposure, never backward.
    pub(super) fn revoke_device(
        &self,
        db: &mut Db,
        device_id: EntityRef,
        reason: RevokeReason,
    ) -> Result<CommandResult, EngineError> {
        let now_ms = self.clock.now_ms();
        let revoked = *device_id.bytes();
        if revoked == self.keychain.device_id() {
            return Err(EngineError::Invalid(
                "a device cannot revoke itself: it would rotate every key away from the only \
                 device holding them"
                    .into(),
            ));
        }
        let op_id = self.fresh_op_id(now_ms);
        let revoke = InnerOp::DeviceRevoke(DeviceRevokePayload {
            revoked_device_id: revoked,
            reason_code: reason,
        });
        let inner = encode_inner_op(&revoke)?;

        // Read inside the transaction, and after the epoch below has been
        // resolved. Resolving it can mint the vault-meta stream's own first key
        // and emit `key_envelope` ops into this very stream, each taking a
        // `seq`; a number read beforehand would already be spent by the time
        // this op reached the log, and `ops` has a
        // `UNIQUE(stream_id, device_id, seq)`. That is the bug `60ee61d`
        // fixed for `emit_control_op`, and this is the same shape.
        let mut seq = 0u64;
        db.with_tx(|tx| -> rusqlite::Result<()> {
            let known: i64 = tx.query_row(
                "SELECT count(*) FROM devices WHERE device_id = ?",
                params![&revoked[..]],
                |r| r.get(0),
            )?;
            if known == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            let hlc = self.hlc.send();
            // The register goes first, before anything can mint a key.
            //
            // `ensure_stream_epoch` below can mint the vault-meta stream's
            // first epoch, and minting emits a `key_envelope` per recipient. If
            // the register were still empty at that moment, the device this
            // transaction exists to revoke would be one of those recipients and
            // would receive a key minted by its own revocation. Ordering the
            // write first is the whole fix; nothing downstream needs the
            // register to be absent.
            self.apply_control_op(tx, &revoke, &self.keychain.device_id(), hlc, now_ms)?;

            // The epoch every rotation op is sealed under: read before
            // anything is minted, so it is the epoch the departing devices and
            // the remaining ones all still share.
            let seal_under = self.ensure_stream_epoch(tx, &META_STREAM, now_ms)?;
            seq = self.next_seq_tx(tx, &META_STREAM)?;

            // 1. The revocation itself, sealed under the old meta epoch like
            //    every other op in this transaction.
            self.ops_insert_at(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                hlc,
                &inner,
                "device.revoke",
                "device",
                Some(&revoked),
                Some(now_ms),
                None,
                now_ms,
                &[],
                seal_under.0,
                &seal_under.1,
            )?;
            // The row is written by `apply_control_op` and by nothing else --
            // it ran above, before the first mint. A second writer here was a
            // real defect and not a tidiness point: this one was an
            // unconditional `UPDATE` while the remote path took a join, so a
            // user revoking an already-revoked device on their own machine
            // moved that machine's cut while every peer kept the other. The
            // local device is then the only replica accepting a window of ops,
            // which is precisely the divergence the register exists to prevent.

            // The relay's half of the revocation, queued rather than called.
            // `revoke_device` has to work with no network -- a device that is
            // gone is the whole scenario -- so the call cannot be part of the
            // command. The row goes in *this* transaction so the intent and the
            // op cannot diverge: a vault that believes it revoked a device and
            // never queued the telling is the failure this whole mechanism
            // exists to prevent. `Core::drain_relay_revocations` makes the call
            // when a session is up, and until then this row is what remembers
            // that it is owed.
            tx.execute(
                "INSERT INTO relay_revocation_intents (device_id, created_at_ms)
                 VALUES (?, ?)
                 ON CONFLICT(device_id) DO NOTHING",
                params![&revoked[..], now_ms],
            )?;

            // 2 + 3. Rotate everything, and seal each new epoch to every
            //        *unrevoked* device. One mechanism holds that, and the
            //        ordering above is what makes it sufficient: the register
            //        row was written before the first mint, and
            //        `emit_key_envelopes` excludes any device with a row. There
            //        is no clock in that path and nothing for a skewed or
            //        restarted one to get wrong.
            //
            //        The exclusion is not cosmetic, because the identity copy
            //        emitted alongside is no longer openable by a device
            //        pairing admitted.
            for stream_id in self.keychain.rotation_set(tx)? {
                let (epoch, key) =
                    self.keychain
                        .mint_epoch(tx, &stream_id, self.rng.as_ref(), now_ms)?;
                self.emit_key_envelopes(tx, &stream_id, epoch, &key, now_ms, Some(&seal_under))?;
            }
            Ok(())
        })
        .map_err(|e| match e {
            sunrise_storage::DbError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
                EngineError::NotFound(format!("device {}", hex_short(&revoked)))
            }
            other => EngineError::Storage(other),
        })?;

        Ok(CommandResult::new(device_id, None, op_id, seq))
    }

    /// Mint a new epoch for one Stream and distribute it.
    ///
    /// The narrow form of what [`Self::revoke_device`] does to everything: used
    /// when a Stream key is believed exposed without a device being at fault,
    /// and as the seam a share-revocation will hang off.
    pub(super) fn rotate_stream_key(
        &self,
        db: &mut Db,
        stream: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        let now_ms = self.clock.now_ms();
        let stream_id = *stream.bytes();
        let mut minted = 0u32;
        // Read inside the transaction, for the reason given in
        // [`Self::revoke_device`]: resolving the meta epoch can emit ops into
        // the very stream this number counts.
        let mut seq = 0u64;
        db.with_tx(|tx| -> rusqlite::Result<()> {
            let seal_under = self.ensure_stream_epoch(tx, &META_STREAM, now_ms)?;
            seq = self.next_seq_tx(tx, &META_STREAM)?;
            let (epoch, key) =
                self.keychain
                    .mint_epoch(tx, &stream_id, self.rng.as_ref(), now_ms)?;
            minted = epoch;
            self.emit_key_envelopes(tx, &stream_id, epoch, &key, now_ms, Some(&seal_under))
        })?;
        Ok(CommandResult::new(
            stream,
            None,
            [0u8; 16],
            u64::from(minted).max(seq),
        ))
    }

    /// Publish this device's identity-signed cert so every replica can verify
    /// its envelopes.
    ///
    /// Emitted by `Core::open` once per vault. It replaces the manual
    /// `Command::TrustDevice` exchange, which accepted a cert from a caller if
    /// it was self-signed — a check every cert passes, including a stranger's.
    ///
    /// # Errors
    /// Storage failures.
    pub fn publish_device_cert(&self, db: &mut Db) -> Result<(), EngineError> {
        let now_ms = self.clock.now_ms();
        let cert = self.keychain.cert_blob().to_vec();
        let device_id = self.keychain.device_id();
        let already: i64 = db.conn().query_row(
            "SELECT count(*) FROM ops WHERE inner_kind = 'device.cert' AND device_id = ?",
            params![&device_id[..]],
            |r| r.get(0),
        )?;
        if already > 0 {
            return Ok(());
        }
        db.with_tx(|tx| -> rusqlite::Result<()> {
            self.emit_control_op(tx, &InnerOp::DeviceCertPublish(cert), now_ms, None)
        })?;
        Ok(())
    }

    /// Apply a remote op envelope: idempotent, entity-level last-writer-wins.
    ///
    /// This is the receive half of sync. The whole pipeline runs under one
    /// `BEGIN IMMEDIATE` transaction (after out-of-tx crypto verification):
    ///
    /// 1. `decode_envelope` — malformed bytes are rejected.
    /// 2. Sender lookup — `envelope.device_id` must be a device this vault has
    ///    admitted, else [`EngineError::UnknownDevice`]. Revocation is **not**
    ///    consulted on this path, and that is deliberate rather than pending:
    ///    refusing here is not convergent, because a replica that applied an op
    ///    before the revocation arrived has no way to un-apply it and this
    ///    engine has no projection rebuild. Two replicas with the same op set
    ///    would disagree forever. **Nothing bounds a revoked device's writes
    ///    today.** The relay would have to be told out of band and cannot be:
    ///    it identifies devices by a ULID it minted at registration, and this
    ///    vault knows only its own device id, so there is no id to name in the
    ///    request. That is
    ///    [#80](https://github.com/justin13888/Sunrise/issues/80); a convergent
    ///    peer-side check is
    ///    [#82](https://github.com/justin13888/Sunrise/issues/82).
    /// 3. `verify_envelope` against the stored device pubkey — a bad signature
    ///    is never applied.
    /// 4. Decrypt under the Stream key for the envelope's `(stream_id, epoch)`
    ///    and decode the inner op. **No key is not an error**: the
    ///    `key_envelope` op carrying it may not have arrived, so the op is
    ///    parked in `deferred_ops` and this returns `Ok(vec![])` without
    ///    reaching any step below. It is retried after every absorbed key.
    /// 5. Clock gate: the envelope's `hlc` is observed into this device's HLC.
    ///    A reading beyond `MAX_DRIFT_MS` in the future is refused outright —
    ///    see [`sunrise_cbor::hlc`].
    /// 6. Idempotence gate: `INSERT OR IGNORE` into `ops` on the deterministic
    ///    op-id and the `UNIQUE(stream_id, device_id, seq)` constraint. If the
    ///    op was already present (`changes() == 0`), return `Ok(None)` with no
    ///    materialization and no event.
    /// 7. LWW materialization: the entity's stored `(hlc, device, seq)` stamp
    ///    is compared against the envelope's. The greater tuple wins. A winning
    ///    op performs the same materialized-row upsert the local path does and
    ///    stamps the row with the SENDER's values; a losing op keeps the row
    ///    but stays recorded in the op log.
    /// 8. Advance `sync_cursors(stream_id, device_id)` over the contiguous
    ///    applied prefix.
    ///
    /// Remote ops are **not** enqueued in the outbox: the relay fans out to
    /// peers, so re-broadcasting a received op would loop.
    ///
    /// Returns the [`DomainEvent`] the caller should broadcast (matching what a
    /// local submit emits for the same op kind), or `Ok(None)` on an idempotent
    /// re-receive.
    ///
    /// # Errors
    /// [`EngineError::RemoteOpInvalid`] for malformed / unverifiable /
    /// undecryptable envelopes, [`EngineError::UnknownDevice`] for an untrusted
    /// sender, and storage errors from the transaction.
    pub fn apply_remote(
        &self,
        db: &mut Db,
        envelope_bytes: &[u8],
    ) -> Result<Option<DomainEvent>, EngineError> {
        Ok(self
            .apply_remote_all(db, envelope_bytes)?
            .into_iter()
            .next())
    }

    /// [`Self::apply_remote`], returning **every** event the delivery produced.
    ///
    /// One envelope can produce more than one, and the case is not exotic: a
    /// `key_envelope` op is itself silent, but absorbing the key it carries
    /// releases every op that had been parked waiting for it. Those ops
    /// materialize now, and a caller that broadcast only the first — or only
    /// the envelope op's own `None` — would leave a screen showing a vault the
    /// database no longer contains. [`crate::Core::apply_remote`] takes this
    /// form for exactly that reason.
    pub fn apply_remote_all(
        &self,
        db: &mut Db,
        envelope_bytes: &[u8],
    ) -> Result<Vec<DomainEvent>, EngineError> {
        // a. Decode.
        let env = decode_envelope(envelope_bytes)
            .map_err(|e| EngineError::RemoteOpInvalid(format!("decode: {e}")))?;

        // b. Sender must be a device this vault has admitted.
        //
        //    One family bypasses the lookup, because it is what *creates* the
        //    row the lookup reads: a `DeviceCertPublish` op carries the
        //    sender's own identity-signed cert, and is self-authenticating —
        //    the envelope signature is checked against the cert's own
        //    `d_s_pub`, and the cert against this vault's account identity. A
        //    stranger's cert fails the second check however it is delivered,
        //    which is exactly what `Command::TrustDevice` could not do.
        //
        //    A *revoked* device's row is found here like any other, and its op
        //    is applied like any other. Revocation is enforced against a
        //    device's *reads* — it is sealed no new epoch — and not against its
        //    writes, which is deliberate and not pending: see this function's
        //    own step 2 above for why refusing here would not converge, and
        //    ADR-0034 (`docs/11-adr/0034-revocation-bounds-reads-not-writes.md`)
        //    for the decision and what would reopen it.
        let d_s_pub = match self.lookup_device_cert(db, &env.device_id)? {
            Some(cert_blob) => {
                let cert = DeviceCert::from_cbor(&cert_blob)
                    .map_err(|e| EngineError::RemoteOpInvalid(format!("stored cert: {e}")))?;
                cert.body.d_s_pub
            }
            None => self.self_authenticating_signer(db, envelope_bytes, &env)?,
        };

        // c. Verify the signature before doing anything else.
        verify_envelope(&env, &d_s_pub)
            .map_err(|e| EngineError::RemoteOpInvalid(format!("verify: {e}")))?;

        // d. Decrypt under whichever key at `(stream_id, epoch)` opens it. Two
        //    devices can have minted that epoch concurrently, so this is a
        //    short list rather than a single key, and the AEAD tag is what
        //    picks — not a stored discriminator that could be lied about.
        //
        //    No key at all is NOT an error: the `key_envelope` op that carries
        //    it may simply not have arrived yet, and the two have no ordering
        //    guarantee across streams. Refusing would lose the op (the relay
        //    does not redeliver), and a cursor barrier would stall the whole
        //    stream. It is parked in `deferred_ops` and retried after every
        //    absorbed key.
        let keys = self.keychain.stream_keys_at(&env.stream_id, env.epoch);
        if keys.is_empty() {
            self.defer_op(db, envelope_bytes, &env)?;
            return Ok(Vec::new());
        }
        let inner_cbor = keys
            .iter()
            .find_map(|k| open_envelope(&env, &d_s_pub, Some(k)).ok())
            .ok_or_else(|| {
                EngineError::RemoteOpInvalid(
                    "no key at this (stream, epoch) opens the envelope".into(),
                )
            })?;
        let mut inner = decode_inner_op(&inner_cbor)
            .map_err(|e| EngineError::RemoteOpInvalid(format!("inner op: {e}")))?;
        remap_legacy_inbox(&mut inner);

        // e. Clock gate. A reading far in OUR future is a broken or hostile
        //    clock; absorbing it would drag this device's HLC forward with the
        //    bad one and let the sender win every conflict for the length of
        //    the skew. A reading in the past is fine and common — that is a
        //    device coming back from a week offline.
        self.hlc
            .observe(env.hlc)
            .map_err(|e| EngineError::RemoteOpInvalid(format!("hlc: {e}")))?;

        // The row is stamped with the SENDER'S hlc and seq, never with this
        // device's post-merge reading. Every replica must record the same stamp
        // for the same op, or the LWW winner would depend on delivery order.
        let lww = LwwStamp {
            hlc: env.hlc,
            device: env.device_id,
            seq: env.seq,
        };

        let now_ms = self.clock.now_ms();
        let op_id = remote_op_id(&env.stream_id, &env.device_id, env.seq);
        let target = inner.target_ref();
        let effect = inner.effect();
        let inner_kind = inner.inner_kind();
        let target_kind = inner.target_kind();

        let mut applied = false;
        let mut absorbed: Vec<([u8; 16], u32)> = Vec::new();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            // e. Idempotence gate.
            OpLog::insert(
                tx,
                &op_id,
                &env.stream_id,
                &env.device_id,
                env.seq,
                env.hlc.physical_ms,
                envelope_bytes,
                inner_kind,
                target_kind,
                Some(target.bytes()),
                Some(now_ms),
                Some(&env.device_id),
                now_ms,
                &[],
            )
            .map_err(|e| match e {
                sunrise_storage::OpLogError::Sqlite(s) => s,
                sunrise_storage::OpLogError::Db(_) => rusqlite::Error::ExecuteReturnedResults,
            })?;
            if tx.changes() == 0 {
                // Already applied: nothing further.
                return Ok(());
            }
            applied = true;
            if inner.is_control() {
                // f'. Control ops carry key material and trust, not entity
                //     state. They have no row and no LWW contest; routing one
                //     into `materialize_remote` would file it under `tasks`,
                //     because that function's kind table ends in a `_ =>` arm.
                absorbed = self.apply_control_op(tx, &inner, &env.device_id, env.hlc, now_ms)?;
            } else {
                // f. LWW materialization.
                materialize_remote(tx, &inner, &lww)?;
            }
            // g. Advance the sync cursor to the end of the contiguous prefix.
            upsert_sync_cursor(tx, &env.stream_id, &env.device_id)?;
            Ok(())
        })?;

        if !applied {
            return Ok(Vec::new());
        }
        let mut events = match effect {
            OpEffect::Create => vec![DomainEvent::Created(target)],
            OpEffect::Update => vec![DomainEvent::Updated(target)],
            OpEffect::Delete => vec![DomainEvent::Deleted(target)],
            // The control op itself changed nothing on screen. What it
            // released might have.
            OpEffect::Control => Vec::new(),
        };
        // A newly absorbed key may be the one a parked op was waiting for.
        for (stream_id, epoch) in absorbed {
            events.extend(self.drain_deferred(db, &stream_id, epoch)?);
        }
        Ok(events)
    }

    /// Recover the signing key for an op from a device this vault has never
    /// seen, when — and only when — the op is that device publishing its own
    /// identity-signed cert.
    ///
    /// Both checks matter. The envelope must be signed by the key the cert
    /// names, which proves the sender holds `D_S_priv`; and the cert must
    /// verify under *this vault's* `ID_S_pub` with an `identity_id` recomputed
    /// from it, which proves the account admitted that device. Either one alone
    /// is bypassable: without the first, anyone can replay someone else's cert;
    /// without the second, any self-signed cert is accepted, which is precisely
    /// the hole `Command::TrustDevice` had.
    fn self_authenticating_signer(
        &self,
        db: &Db,
        envelope_bytes: &[u8],
        env: &sunrise_crypto::OpEnvelope,
    ) -> Result<[u8; 32], EngineError> {
        let _ = envelope_bytes;
        // The payload is only readable once we have a key for the stream, and
        // we only have one if the sender is inside the account already. That is
        // the point: the cert travels in the vault-meta stream, sealed under a
        // key only members hold, so a stranger cannot even present one.
        let keys = self.keychain.stream_keys_at(&env.stream_id, env.epoch);
        for key in &keys {
            // Verification is deferred to the caller, so open without checking
            // the signature first — the payload is authenticated by the AEAD
            // tag regardless, and the signature check follows immediately.
            let Ok(inner_cbor) = open_envelope_unverified(env, key) else {
                continue;
            };
            let Ok(InnerOp::DeviceCertPublish(cert_cbor)) = decode_inner_op(&inner_cbor) else {
                continue;
            };
            let cert = DeviceCert::from_cbor(&cert_cbor)
                .map_err(|e| EngineError::RemoteOpInvalid(format!("published cert: {e}")))?;
            if cert.body.device_id != env.device_id {
                return Err(EngineError::RemoteOpInvalid(
                    "published cert names another device".into(),
                ));
            }
            cert.verify_binding(
                &self.keychain.identity_signing_pub(),
                &self.keychain.identity_id(),
            )
            .map_err(|e| EngineError::RemoteOpInvalid(format!("published cert: {e}")))?;
            let _ = db;
            return Ok(cert.body.d_s_pub);
        }
        Err(EngineError::UnknownDevice)
    }

    /// Whether `device_id` is revoked: **is there a row**, and nothing else.
    ///
    /// The read half of revocation calls this from
    /// [`Self::backfill_key_envelopes`], and the same presence test is inlined
    /// as an anti-join in [`Self::emit_key_envelopes`], which needs it per row
    /// rather than per call.
    ///
    /// # Why there is no time comparison here
    ///
    /// There was one — `cut_ms <= at_ms` — and it never decided anything except
    /// wrongly. `cut_ms` is the revoking device's HLC physical half, so the
    /// only right-hand side comparable with it is this device's own HLC
    /// reading, which is at or above every peer stamp it has absorbed and
    /// therefore at or above every cut it has recorded. In one process the test
    /// is *always true*, which is fail-closed-on-presence written the long way.
    ///
    /// The one case where it did change an outcome is the case it got wrong.
    /// [`MonotonicHlc`](crate::config::MonotonicHlc) is deliberately not
    /// persisted, [`Engine::from_clock`] builds a fresh one at `Hlc::default()`,
    /// and nothing primes it from the op log or the register at open. So after
    /// any restart the HLC reads 0 while `cut_ms` rows survive, and the
    /// comparison silently collapsed to the bare wall clock it was introduced
    /// to replace — reopening the leak for every cut stamped ahead of local
    /// time, which a peer inside `MAX_DRIFT_MS` produces routinely and a
    /// backgrounded mobile app restarts into as a matter of course.
    ///
    /// So the row is the whole of it. `cut_ms` and `cut_logical` are untouched
    /// and still load-bearing: they are the LWW discriminator deciding *which*
    /// revocation wins when two race (see the `DeviceRevoke` arm of
    /// [`Self::apply_control_op`]). They simply do not gate whether a recorded
    /// revocation applies.
    ///
    /// This is the *local* half of a larger bound: the register is per-replica,
    /// so a device that has not yet applied the `device_revoke` op has no row
    /// to read at all and will seal a new epoch to the revoked device. That is
    /// propagation, and no comparison could ever have closed it.
    pub(super) fn is_revoked(
        &self,
        conn: &rusqlite::Connection,
        device_id: &[u8; 16],
    ) -> rusqlite::Result<bool> {
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM device_revocations WHERE device_id = ?",
                params![&device_id[..]],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Park an op whose Stream key has not arrived yet.
    ///
    /// Bounded by [`DEFERRED_PER_EPOCH_CAP`] and [`DEFERRED_TOTAL_CAP`]. This
    /// runs before anything about the payload has been checked — the key
    /// that would open it is precisely what is missing — so the only thing
    /// standing between a member and unbounded storage on every peer is the
    /// two caps. Overflow evicts oldest-first; see [`DEFERRED_TOTAL_CAP`] for
    /// why that direction and not the other.
    ///
    /// # Why `env.hlc` is deliberately not observed here
    ///
    /// [`Self::apply_remote_all`] absorbs a peer's stamp at its step (e), which
    /// is *after* the decrypt at step (d) — so an op that parks here returns
    /// before `self.hlc.observe(env.hlc)` and its stamp does not enter this
    /// device's clock until its key arrives and [`Self::drain_deferred`] retries
    /// it through the whole path. That omission is the intended behaviour, and
    /// there are three reasons it is (issue #157):
    ///
    /// 1. **It would not survive an open.** [`Self::prime_hlc`] restores the
    ///    clock from `ops`, and a parked op is in `deferred_ops`, which has no
    ///    stamp column and is not read at open. Observing it would put the
    ///    process above a reading the next restart cannot reproduce — an
    ///    invented, unrepeatable clock position rather than a restored one.
    /// 2. **A parked op is not necessarily an op at all.** It expires at
    ///    `DEFERRED_TTL_MS`, it is evicted when either cap overflows, and a
    ///    drained op that still will not apply is dropped. Absorbing the stamp
    ///    of ciphertext this replica may never open drags every subsequent LWW
    ///    comparison forward on the strength of a byte string it cannot read.
    /// 3. **The clock bounds what was applied, not what arrived.** That is
    ///    exactly what makes it usable: everything this device emits sorts above
    ///    everything it has *acted on*. An op it cannot decrypt is not one it
    ///    has acted on, and the retry re-runs the full path, so nothing is lost.
    ///
    /// What this costs is that between arrival and drain — unbounded, for a
    /// device offline across a rotation — this replica holds a stamp on disk
    /// that its clock does not reflect. Nothing reads [`HlcClock::peek`](crate::config::HlcClock::peek) for a
    /// decision today; revocation did for one revision and
    /// [`Self::is_revoked`] records why it stopped. Anything that starts
    /// comparing against the local reading again has to answer this window
    /// first.
    fn defer_op(
        &self,
        db: &mut Db,
        envelope_bytes: &[u8],
        env: &sunrise_crypto::OpEnvelope,
    ) -> Result<(), EngineError> {
        let op_id = remote_op_id(&env.stream_id, &env.device_id, env.seq);
        let now_ms = self.clock.now_ms();
        let mut evicted = 0usize;
        db.with_tx(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO deferred_ops
                 (op_id, stream_id, epoch, envelope, received_at_ms)
                 VALUES (?, ?, ?, ?, ?)",
                params![
                    &op_id[..],
                    &env.stream_id[..],
                    env.epoch,
                    envelope_bytes,
                    now_ms
                ],
            )?;
            // `received_at_ms` alone does not order rows within one clock
            // millisecond, so `op_id` breaks the tie and keeps the eviction
            // total rather than arbitrary.
            evicted += tx.execute(
                "DELETE FROM deferred_ops WHERE op_id IN (
                     SELECT op_id FROM deferred_ops
                     WHERE stream_id = ?1 AND epoch = ?2
                     ORDER BY received_at_ms DESC, op_id DESC
                     LIMIT -1 OFFSET ?3
                 )",
                params![&env.stream_id[..], env.epoch, DEFERRED_PER_EPOCH_CAP],
            )?;
            evicted += tx.execute(
                "DELETE FROM deferred_ops WHERE op_id IN (
                     SELECT op_id FROM deferred_ops
                     ORDER BY received_at_ms DESC, op_id DESC
                     LIMIT -1 OFFSET ?1
                 )",
                params![DEFERRED_TOTAL_CAP],
            )?;
            Ok(())
        })?;
        if evicted > 0 {
            tracing::warn!(
                ev = "core.op.deferred_evicted",
                n_dropped = evicted,
                stream_h = hex_short(&env.stream_id),
                "the parked-op buffer is full; the oldest entries were dropped"
            );
        }
        Ok(())
    }

    /// Re-apply every op parked against `(stream_id, epoch)`.
    ///
    /// Called after every absorbed key. A drained op takes the ordinary
    /// `apply_remote` path, so it goes through the same trust, clock and LWW
    /// gates it would have on first delivery; it is only its *arrival order*
    /// that was wrong. The row is deleted before the retry so a permanently
    /// unopenable op cannot make every subsequent absorb replay it forever.
    ///
    /// What that ordering costs is a gap with no transaction over it. The TTL
    /// sweep is one `with_tx`, the bucket delete is a second, and the
    /// [`Self::apply_remote_all`] loop runs outside both — so a crash after the
    /// delete and before the loop finishes loses the parked envelopes on this
    /// replica. Recovery is the one [`DEFERRED_TOTAL_CAP`] already relies on for
    /// an evicted op, and for the same reason: a parked op never reached `ops`
    /// and never advanced the sync cursor, so the relay still counts it as
    /// undelivered and re-sends it on the next reconnect — by which time the key
    /// that opens it is already here.
    fn drain_deferred(
        &self,
        db: &mut Db,
        stream_id: &[u8; 16],
        epoch: u32,
    ) -> Result<Vec<DomainEvent>, EngineError> {
        let parked: Vec<Vec<u8>> = {
            let mut stmt = db.conn().prepare(
                "SELECT envelope FROM deferred_ops
                 WHERE stream_id = ? AND epoch = ? ORDER BY received_at_ms",
            )?;
            let rows = stmt
                .query_map(params![&stream_id[..], epoch], |r| r.get::<_, Vec<u8>>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        // The age sweep runs on **every** absorbed key, not only on one that
        // releases something. It used to sit past the early return below, so it
        // ran only when this exact `(stream, epoch)` had rows parked — which is
        // precisely the bucket about to be emptied anyway. A row ages out
        // because nothing ever arrives for its bucket, so the one condition
        // that reached the sweep was the one condition under which there was
        // nothing to sweep. See [`DEFERRED_TTL_MS`] for why a row this old is
        // ciphertext nobody will ever open.
        let cutoff =
            i64::try_from(self.clock.now_ms().saturating_sub(DEFERRED_TTL_MS)).unwrap_or(i64::MAX);
        db.with_tx(|tx| {
            tx.execute(
                "DELETE FROM deferred_ops WHERE received_at_ms < ?",
                params![cutoff],
            )?;
            Ok(())
        })?;
        if parked.is_empty() {
            return Ok(Vec::new());
        }
        db.with_tx(|tx| {
            tx.execute(
                "DELETE FROM deferred_ops WHERE stream_id = ? AND epoch = ?",
                params![&stream_id[..], epoch],
            )?;
            Ok(())
        })?;
        let mut events = Vec::new();
        for envelope in parked {
            // A drained op that still cannot be applied is dropped rather than
            // failing the absorb that released it: the key arrived correctly,
            // and one bad op must not undo that.
            if let Ok(more) = self.apply_remote_all(db, &envelope) {
                events.extend(more);
            }
        }
        Ok(events)
    }

    /// Apply one control op. Returns the `(stream_id, epoch)` pairs whose keys
    /// this device newly learned, so the caller can drain their parked ops.
    pub(super) fn apply_control_op(
        &self,
        tx: &Transaction<'_>,
        inner: &InnerOp,
        sender: &[u8; 16],
        hlc: Hlc,
        now_ms: u64,
    ) -> rusqlite::Result<Vec<([u8; 16], u32)>> {
        match inner {
            InnerOp::KeyEnvelope(p) => {
                // The epoch bound runs first, ahead of the recipient match,
                // because the third-party arm below writes a
                // `key_envelope_recipients` row and returns before any other
                // check in this arm can run. An epoch far above this replica's
                // live one is refused before it can be written anywhere.
                //
                // Two distinct harms, one bound. `MAX(epoch)` is what makes a
                // key live, so *absorbing* an absurd epoch strands `mint_epoch`
                // at the saturation point and redirects every op this device
                // seals afterwards to a key nobody else holds. And *recording*
                // an absurd epoch tells `backfill_key_envelopes` that a device
                // nobody has served already holds that epoch's key, so it emits
                // nothing and the device is left unable to read the stream. See
                // [`MAX_EPOCH_LEAP`].
                let live = self
                    .keychain
                    .current_epoch_tx(tx, &p.stream_id)?
                    .unwrap_or(0);
                if p.epoch > live.saturating_add(MAX_EPOCH_LEAP) {
                    tracing::warn!(
                        ev = "core.key.epoch_refused",
                        stream_h = hex_short(&p.stream_id),
                        epoch = p.epoch,
                        live_epoch = live,
                        "a key envelope names an epoch too far above this vault's own"
                    );
                    return Ok(Vec::new());
                }
                let recipient = match p.recipient {
                    Recipient::Device(id) if id == self.keychain.device_id() => {
                        EnvelopeRecipient::Device
                    }
                    // The identity copy is opened too. A device that already
                    // holds the key learns nothing; one that was paired from a
                    // recovery blob learns everything, and the alternative is a
                    // second op family that says the same thing.
                    Recipient::Identity(_) => EnvelopeRecipient::Identity,
                    // Somebody else's copy. Retained in the log — the relay
                    // fans out to every device — and not opened. It *is*
                    // recorded: this is how a device learns that some other
                    // device has already sealed this epoch to that recipient,
                    // which is what stops every replica emitting the same
                    // backfill envelope.
                    //
                    // This is a claim the recorder cannot check: it holds no
                    // key to open the ciphertext with. A member emitting a
                    // `key_envelope` full of garbage addressed to a third
                    // device suppresses that device's backfill for that
                    // `(stream, epoch)` -- `backfill_key_envelopes` finds a
                    // row, emits nothing, and the device holds no key, so its
                    // ops park in `deferred_ops` until `DEFERRED_TTL_MS` drops
                    // them with nothing surfacing. The harm is a withhold, not
                    // a leak.
                    //
                    // What bounds it is the `MAX_EPOCH_LEAP` check above, which
                    // now runs before this row is written. Two bounds this
                    // comment used to claim were not bounds and are gone:
                    //
                    // * "the next rotation corrects it, because that is a new
                    //   epoch this table has no row for" -- false while any
                    //   epoch could be claimed. Rows filed for e+1, e+2, ...
                    //   poison rotations that have not happened yet, and the
                    //   correction never arrives.
                    // * "a member who can do this can read everything already"
                    //   -- false for exactly the member class revocation now
                    //   creates. A revoked device's reads are bounded by the
                    //   rotation and its writes are bounded by nothing (see
                    //   [`Self::apply_remote`] step 2), so it can file these
                    //   rows and cannot read what they withhold.
                    //
                    // The residual is a claim inside the leap window, which is
                    // corrected by the first rotation past `live +
                    // MAX_EPOCH_LEAP`. Closing it outright would need the
                    // recorder to verify a ciphertext it holds no key for.
                    Recipient::Device(other) => {
                        record_envelope_recipient(tx, &p.stream_id, p.epoch, &other, now_ms)?;
                        return Ok(Vec::new());
                    }
                };
                let Ok(key) = self.keychain.open_key_envelope(
                    recipient,
                    &p.stream_id,
                    p.epoch,
                    &p.hpke_ciphertext,
                ) else {
                    return Ok(Vec::new());
                };
                // The `key_id` is a routing hint, so it is re-derived from the
                // opened key rather than trusted. A mismatch means the sender
                // is confused or hostile; either way the key itself is what
                // opens ops, and it is filed under its real id.
                if stream_key_id(&key) != p.key_id {
                    return Ok(Vec::new());
                }
                let learned = self.keychain.absorb_stream_key(
                    tx,
                    &p.stream_id,
                    p.epoch,
                    &key,
                    KeySource::Envelope,
                    self.rng.as_ref(),
                    now_ms,
                )?;
                // Recorded *here*, and not where the recipient was matched.
                // Everything between the two is a way for this envelope to be
                // refused — an unopenable ciphertext, a `key_id` that does not
                // re-derive — and a row written before those runs would say
                // "this device has the key" about a key it declined. Nothing
                // re-sends against a row that is already there, so that mistake
                // is not self-correcting. (The epoch bound is the exception: it
                // runs at the top of this arm, because the third-party arm
                // records and returns before reaching here.)
                record_envelope_recipient(
                    tx,
                    &p.stream_id,
                    p.epoch,
                    &self.keychain.device_id(),
                    now_ms,
                )?;
                Ok(if learned {
                    vec![(p.stream_id, p.epoch)]
                } else {
                    Vec::new()
                })
            }
            InnerOp::DeviceRevoke(p) => {
                // The revocation row is an **LWW register** keyed on the op's
                // own `(hlc, device_id)` — the same rule ADR-0014 resolves
                // every other concurrent write in this engine with, rather than
                // a bespoke one for this family.
                //
                // The cut is that same HLC. There is no `effective_at` field to
                // pick a winner between, which is the point: see
                // [`DeviceRevokePayload`] for the two bounds that could not be
                // made to hold on an emitter-chosen one.
                //
                // Why LWW and not "earliest cut wins", which this arm briefly
                // did: `MIN` converges, but it is **irreversible**. A cut that
                // lands too far in the past — which a device with a slow clock
                // produced through the ordinary command, no crafted input
                // needed — could never be corrected, and it refuses its
                // target's entire history on every replica. Under LWW a later
                // revocation supersedes an earlier one, so a bad cut is fixed
                // by revoking again from a healthy device.
                //
                // `logical` is in the key because an HLC is `(physical,
                // logical)` and comparing physical alone would drop the half
                // that orders two ops inside one millisecond. `revoked_by`
                // breaks the remaining tie the way `LwwStamp` does, so
                // `revoke_reason` and `revoked_by` follow the winning op
                // instead of being order-dependent alongside a converged
                // timestamp.
                // A device cannot revoke itself, and cannot move its own
                // cut. Register hygiene independent of any gate: a device that
                // can rewrite its own row can push its cut forward and undo a
                // revocation somebody else made of it, which is the one edit
                // the register must never accept from the party it is about.
                // `Command::RevokeDevice` refuses it locally for the separate
                // reason that rotating every key away from the only device
                // holding them is not a recoverable state; this is the remote
                // half, and neither implies the other.
                if p.revoked_device_id == *sender {
                    tracing::warn!(
                        ev = "core.device.revoke_refused",
                        reason = "self",
                        sender_h = hex_short(sender),
                        "a device tried to move its own revocation cut"
                    );
                    return Ok(Vec::new());
                }
                let cut = i64::try_from(hlc.physical_ms).unwrap_or(i64::MAX);
                let logical = i64::from(hlc.logical);
                // Its own table, keyed on the revoked device id, so a
                // revocation naming a device this replica has never seen is
                // durable without inventing one. That case is ordinary: the
                // cert travels in the same stream with no ordering guarantee,
                // and one parked in `deferred_ops` at meta epoch k drains after
                // a revocation absorbed at k+1.
                //
                // Upserting into `devices` handled it and cost too much — every
                // such op minted a row there, so revocations of ids nobody
                // knows became phantom entries in the user's device list, and a
                // phantom row also satisfied `revoke_device`'s "is this device
                // known" guard, which would mint a fresh epoch for every stream
                // in the account on the way to revoking a ghost.
                tx.execute(
                    "INSERT INTO device_revocations
                     (device_id, cut_ms, cut_logical, revoked_by, reason, recorded_at_ms)
                     VALUES (?4, ?1, ?2, ?3, ?5, ?6)
                     ON CONFLICT(device_id) DO UPDATE SET
                        cut_ms = ?1,
                        cut_logical = ?2,
                        revoked_by = ?3,
                        reason = ?5,
                        recorded_at_ms = ?6
                     WHERE (?1, ?2, ?3) > (cut_ms, cut_logical, revoked_by)",
                    params![
                        cut,
                        logical,
                        &sender[..],
                        &p.revoked_device_id[..],
                        p.reason_code.as_str(),
                        i64::try_from(now_ms).unwrap_or(i64::MAX),
                    ],
                )?;
                Ok(Vec::new())
            }
            InnerOp::DeviceCertPublish(cert_cbor) => {
                // Verified in `self_authenticating_signer` before we ever got
                // here for an unknown sender; re-verified here because a known
                // sender's op takes the ordinary path and could otherwise
                // publish an unchecked cert for a third device.
                //
                // Every rejection below is logged rather than returned. The
                // caller is `apply_remote`, which has already accepted the
                // envelope — the signature verified and the sender is a member
                // — so failing the whole delivery would put a well-formed op
                // into the refusal path over a payload defect. But dropping it
                // *silently* is how a `d_d_pub` stays NULL forever while the
                // vault looks healthy: the device is never a `key_envelope`
                // recipient, its peers' ops park in `deferred_ops`, and nothing
                // anywhere says why.
                let Ok(cert) = DeviceCert::from_cbor(cert_cbor) else {
                    tracing::warn!(
                        ev = "core.device.cert_rejected",
                        reason = "undecodable",
                        sender_h = hex_short(sender),
                        "a published device cert did not decode"
                    );
                    return Ok(Vec::new());
                };
                // A cert is published by the device it names, and by no one
                // else. Without this, any member could rebind a sibling's
                // `cert_blob` — and with it that sibling's `d_s_pub` and
                // `d_d_pub` — through the `ON CONFLICT DO UPDATE` below: a
                // converging denial of service on the sibling's future ops and
                // a redirect of its future `key_envelope`s. The unknown-sender
                // path in `self_authenticating_signer` has always made this
                // check; the two paths now agree.
                if cert.body.device_id != *sender {
                    tracing::warn!(
                        ev = "core.device.cert_rejected",
                        reason = "names_another_device",
                        sender_h = hex_short(sender),
                        subject_h = hex_short(&cert.body.device_id),
                        "a device published a cert naming another device"
                    );
                    return Ok(Vec::new());
                }
                if cert
                    .verify_binding(
                        &self.keychain.identity_signing_pub(),
                        &self.keychain.identity_id(),
                    )
                    .is_err()
                {
                    tracing::warn!(
                        ev = "core.device.cert_rejected",
                        reason = "binding",
                        sender_h = hex_short(sender),
                        "a published device cert does not verify under this account identity"
                    );
                    return Ok(Vec::new());
                }
                // A device id nobody has seen before, appearing in an
                // account that has revoked something, is the observable
                // signature of the one bypass revocation does not close: a
                // revoked device still holds `ID_S_priv`, so it can mint a
                // fresh id, sign a valid cert for it, and rejoin under a name
                // the register does not list
                // ([#105](https://github.com/justin13888/Sunrise/issues/105)).
                //
                // It is the signature of an ordinary pairing too, and nothing
                // here can tell the two apart — that is exactly what #105 is,
                // and ADR-0032 records why no check available today separates
                // them without either over-blocking honest devices or diverging
                // replicas. So this discloses and does not gate: the cert is
                // applied either way, every replica applies it, and the account
                // converges. What changes is that the event exists to be seen.
                let readmission = {
                    let known: bool = tx
                        .query_row(
                            "SELECT 1 FROM devices WHERE device_id = ?",
                            params![&cert.body.device_id[..]],
                            |_| Ok(()),
                        )
                        .optional()?
                        .is_some();
                    let revocations: i64 =
                        tx.query_row("SELECT count(*) FROM device_revocations", [], |r| r.get(0))?;
                    !known && revocations > 0
                };
                if readmission {
                    tracing::warn!(
                        ev = "core.device.admitted_after_revocation",
                        sender_h = hex_short(sender),
                        subject_h = hex_short(&cert.body.device_id),
                        "a device id this vault has never seen joined an account that \
                         has revoked a device"
                    );
                }
                tx.execute(
                    "INSERT INTO devices
                     (device_id, cert_blob, nickname, platform, created_at_ms,
                      identity_id, d_d_pub)
                     VALUES (?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT(device_id) DO UPDATE SET
                        cert_blob = excluded.cert_blob,
                        nickname = excluded.nickname,
                        platform = excluded.platform,
                        identity_id = excluded.identity_id,
                        d_d_pub = excluded.d_d_pub",
                    params![
                        &cert.body.device_id[..],
                        cert_cbor,
                        cert.body.nickname,
                        cert.body.platform,
                        i64::try_from(cert.body.created_at_ms).unwrap_or(i64::MAX),
                        &cert.body.identity_id[..],
                        &cert.body.d_d_pub[..],
                    ],
                )?;
                // The device is a member as of this line, so anything this
                // vault holds and it does not is now a gap it cannot close by
                // itself: `ID_D_priv` used to be its way out and is gone.
                //
                // Failing the whole delivery over a backfill would be wrong --
                // the cert is valid and belongs in `devices` whatever happens
                // next -- but swallowing the error would leave the gap open
                // with nothing said, which is the failure mode this arm's own
                // comment above warns about. So it is logged and the cert
                // stands. Nothing retries it: `publish_device_cert` is guarded
                // once per vault, so a replica applies a given device's cert
                // once and this runs once. What does recover the device is the
                // next rotation of the affected stream -- `emit_key_envelopes`
                // seals a fresh epoch to every unrevoked device -- so it regains
                // access to new content and not to the epoch it missed. Another
                // online replica's backfill covers it too, which is the main
                // reason every replica runs one rather than an elected leader.
                if let Err(e) = self.backfill_key_envelopes(
                    tx,
                    &cert.body.device_id,
                    &cert.body.d_d_pub,
                    now_ms,
                ) {
                    tracing::warn!(
                        ev = "core.device.backfill_failed",
                        reason = "storage",
                        subject_h = hex_short(&cert.body.device_id),
                        cause = %e,
                        "could not seal held stream keys to a newly certified device"
                    );
                }
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }

    /// The stored cert for `device_id`, revoked or not. `None` = a device this
    /// vault has never admitted.
    ///
    /// Membership is not consulted here and must not be: this answers "which
    /// key verifies this signature", which is a fact about the device and not
    /// about its standing. Filtering it on revocation once made revocation
    /// retroactive — with the row hidden, every op from a revoked device fell
    /// through to [`Self::self_authenticating_signer`], which knows only
    /// `DeviceCertPublish`, and came back `UnknownDevice` whatever its HLC
    /// said, including work the device did honestly months earlier.
    ///
    /// A revocation affects nothing **on this path**, and that is the whole
    /// point of the paragraph above: this answers "which key verifies this
    /// signature", which is a fact about the device rather than its standing.
    ///
    /// It does affect other paths. [`Self::emit_key_envelopes`] will not seal
    /// a new epoch to a revoked device and [`Self::backfill_key_envelopes`]
    /// will not hand one its keys back, which together are what stop it reading
    /// anything written after the cut.
    pub(super) fn lookup_device_cert(
        &self,
        db: &Db,
        device_id: &[u8; 16],
    ) -> Result<Option<Vec<u8>>, EngineError> {
        let blob: Option<Vec<u8>> = db
            .conn()
            .query_row(
                "SELECT cert_blob FROM devices WHERE device_id = ?",
                params![&device_id[..]],
                |r| r.get(0),
            )
            .optional()?;
        Ok(blob)
    }
}
