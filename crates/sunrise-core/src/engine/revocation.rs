//! Device revocation: the register, the ledger it folds from, and the command
//! that writes both.
//!
//! Split out of [`super::sync`] because it is the one family in that module
//! whose state is *derived* rather than applied. Everything else there
//! materialises a row from an op it has just verified; `device_revocations` is
//! recomputed from `device_revoke_ops` on every change, so that whether a
//! revocation is believed cannot depend on the order this replica happened to
//! receive things in. That difference is the cohesion boundary, and ADR-0041
//! (`docs/11-adr/0041-peer-side-revocation-is-a-fold.md`) is why it exists.
//!
//! # Two questions, two tables
//!
//! This module keeps them apart on purpose, and which one a caller wants
//! decides which it reads.
//!
//! - **"Is this device currently called revoked?"** — `device_revocations`,
//!   via [`Engine::is_revoked`]. Derived, so it is *not* monotone: the fold
//!   rebuilds it from the ledger and a row can leave it when a revocation's
//!   author turns out to have been revoked first. It has to converge on the op
//!   set, because it is what the device list paints and what ADR-0034
//!   corollary 3 requires two replicas holding the same ops to agree on. **Two**
//!   sites ask it: the device list, which reads `device_revocations` directly
//!   through a `LEFT JOIN` rather than through this function (`super::query`),
//!   and `CommandResult::revocation_gated` in [`Engine::revoke_device`] below.
//! - **"Is this device read-bounded?"** — `device_read_bounds`, via
//!   [`Engine::is_read_bounded`]. A ratchet: written only by the fold's
//!   `INSERT OR IGNORE`, never deleted for a device with a cert here. It has
//!   to be monotone or it is not a bound; it gates all four key sites —
//!   `emit_key_envelopes`' anti-join and `backfill_key_envelopes`' early
//!   return in [`super::oplog`], the survivor roster `rotate_identity` builds
//!   in [`super::identity`], and the readmission signal the
//!   `DeviceCertPublish` arm computes in [`super::sync`]. A fifth site asks it
//!   for the same reason without distributing a key: a sender's authority to
//!   claim a third-party `key_envelope` recipient row (`super::sync`), whose
//!   own record names the bounded class rather than the revoked one.
//!
//!   Monotone is **not** convergent, and the difference is load-bearing: the
//!   bound ratchets over the registers *this* replica computed, so two
//!   replicas holding one ledger in different arrival orders can settle on
//!   permanently different bounds. [`Engine::refold_device_revocations`] works
//!   the counterexample; [#282](https://github.com/justin13888/Sunrise/issues/282)
//!   holds the unclosed half.
//!
//! They were one table until `migrations/0028_device_read_bounds.sql`, and
//! while they were, an ordinary unwind released the read bound: the device
//! re-entered every recipient set and one republished cert handed it every
//! held epoch of every stream.

use super::ids::{hex_bytes, hex_short};
use super::{Engine, EngineError, META_STREAM};
use crate::commands::CommandResult;
use crate::control_op::{DeviceRevokePayload, RevokeReason};
use crate::inner_op::{encode_inner_op, InnerOp};
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::BTreeSet;
use sunrise_cbor::hlc::Hlc;
use sunrise_id::EntityRef;
use sunrise_storage::Db;

/// One `device_revoke` op as `device_revoke_ops` keeps it.
///
/// Blobs rather than `[u8; 16]`: `revoked_by` carried over from a pre-0017
/// vault is the empty blob, which names no device and must stay the empty blob
/// rather than become sixteen invented bytes.
struct RevokeLedgerRow {
    hlc_ms: i64,
    hlc_logical: i64,
    sender: Vec<u8>,
    revoked: Vec<u8>,
    reason: String,
    recorded_at_ms: i64,
}

/// A ledger row the fold stored and did not believe, identified by the op that
/// wrote it. Returned so the caller can say so about the op it has just
/// applied, and not about the whole history it re-folded on the way.
struct SkippedRevoke {
    sender: Vec<u8>,
    hlc_ms: i64,
    hlc_logical: i64,
}

impl Engine {
    /// Record a device as revoked, and mint a new epoch for every stream it
    /// could read.
    ///
    /// **What this bounds directly is the revoked device's reads.** Its
    /// *entity* writes are bounded elsewhere and later: the same transaction
    /// queues a `relay_revocation_intents` row, which the sync driver drains
    /// into the relay's `DELETE`, and no peer refuses one of those ops — see
    /// [`Self::apply_remote`] step 2 for why refusing would not converge. Two
    /// of its *control* writes **are** refused at the peer, by
    /// [`Self::refold_device_revocations`] and by the `key_envelope` arm of
    /// [`Self::apply_control_op`]; ADR-0041
    /// (`docs/11-adr/0041-peer-side-revocation-is-a-fold.md`).
    ///
    /// It records a cut every replica converges on, rotates
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
    ///
    /// # When the fold discards this command's own op
    ///
    /// It can: this device may itself have been revoked, and
    /// [`Self::refold_device_revocations`] judges the sender of every row
    /// including the one written a line ago. The command still succeeds and
    /// still returns `Ok` — the op is emitted and stored before the fold
    /// decides, so an error after a committed transaction would be a lie — but
    /// it says so, on [`CommandResult::revocation_gated`], and **nothing else
    /// happens**: no relay intent is queued, no stream is rotated, and the
    /// account identity is left alone.
    ///
    /// Telling the relay to cut a device every replica still shows as current
    /// is the disclosure failure #160 fixed in the other direction. Rotating
    /// is worse than pointless, because the device doing the minting is the
    /// revoked one: it writes each fresh key into its own `stream_keys` and
    /// seals it to every honest peer, which then seal their next writes under
    /// an epoch it holds. The guard on the rotation loop below is where that
    /// is stated in full.
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
        // Rows the rotation below could not reach. Hoisted out of the closure
        // because it has to outlive the transaction and reach the caller: see
        // `CommandResult::unrotated_streams`.
        let mut unrotatable: Vec<Vec<u8>> = Vec::new();
        // Devices the account no longer calls revoked while still giving them
        // no keys. Hoisted for the same reason as `unrotatable`, and read at
        // the end of the transaction because applying this op is one of the
        // things that can produce one. See
        // `CommandResult::revocation_unwound`.
        let mut unwound: Vec<Vec<u8>> = Vec::new();
        // Whether the register believes this command. Hoisted for the same
        // reason as `unrotatable`: it is read inside the transaction and has
        // to reach the caller, on `CommandResult::revocation_gated`.
        let mut effective = true;
        // There is deliberately **no** "this device is revoked, refuse the
        // command" guard here, though the fold in
        // [`Self::refold_device_revocations`] will skip most of what such a
        // device emits. The one revocation it must still be able to make is of
        // the device that revoked it — that is the fold's mutual exception, and
        // it is the recovery path when a compromised device gets its
        // revocation in first. A local guard would close the recovery path to
        // buy a clearer error for the case that does not matter.
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
            // The epoch this transaction's ops will be sealed under, read
            // before anything is minted — the same value `seal_under` below
            // resolves to, and the one a peer will see as `env.epoch`.
            let at_epoch = self
                .keychain
                .current_epoch_tx(tx, &META_STREAM)?
                .unwrap_or(0);
            self.apply_control_op(
                tx,
                &revoke,
                &self.keychain.device_id(),
                hlc,
                now_ms,
                at_epoch,
            )?;

            // **Did the fold believe it?** `apply_control_op` returns `Ok(())`
            // whether the op was folded into the register or stored and
            // skipped, so without this read the command cannot tell the two
            // apart and neither can its caller. The predicate is the register
            // itself, read after the fold has run: it is false exactly when
            // this op was discarded *and* no earlier revocation of the target
            // survives, which is "this command had no effect".
            //
            // Nothing is rolled back and no error is raised. The op has been
            // emitted and stored by the time the fold decides, so returning
            // `Err` after a committed transaction would be a lie, and rolling
            // back would drop the user's act in silence and edge toward the
            // local "refuse if revoked" guard ADR-0041 §Decision 3 refuses on
            // purpose. What changes is what is *claimed*: see
            // `CommandResult::revocation_gated`. The warn line the caller's
            // operator sees is `core.device.revoke_refused` with
            // `reason = "revoked_sender"`, emitted by
            // [`Self::apply_device_revoke`] above — this path mints no event
            // of its own, because the fact is the same fact.
            effective = self.is_revoked(tx, &revoked)?;

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
            //
            // Guarded on the register, because the two halves of a revocation
            // must not disagree about whether there is one. When the fold
            // discards this op there is no cut on any replica, every device
            // list goes on showing the target as current, and it goes on
            // receiving new epochs — while an unguarded intent would drain
            // into the relay's `DELETE` and have it 401 that device. That is
            // the disclosure failure #160 fixed in the other direction, with
            // the relay ahead of the register instead of behind it.
            if effective {
                tx.execute(
                    "INSERT INTO relay_revocation_intents (device_id, created_at_ms)
                     VALUES (?, ?)
                     ON CONFLICT(device_id) DO NOTHING",
                    params![&revoked[..], now_ms],
                )?;
            }

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
            //        `rotation_set` returns two lists: the streams it can
            //        rotate and the rows it cannot. The second is not dropped
            //        here — a row with a malformed `stream_id` is a stream the
            //        revoked device goes on reading, and returning success over
            //        it is the shape this whole path exists to refuse. It is
            //        carried out to the caller instead.
            //
            // **Guarded on `effective`, and the party this protects is not the
            // target.** When the fold discards this command's own op the
            // account records no revocation, nothing is cut, and the target
            // would be a recipient of the new epochs either way — so on the
            // target alone the rotation is merely pointless. What makes it
            // harmful is who is holding the pen. The only way `effective` is
            // false is that *this* device has itself been revoked, and
            // `mint_epoch` writes the fresh key straight into this device's
            // own `stream_keys`, `emit_key_envelopes` seals it to every
            // unbounded peer, `Keychain::absorb_stream_key` stores it with no
            // check on the sender's standing, and `current_epoch_tx` is
            // `MAX(epoch)`. An expelled device running
            // `sunrise devices revoke <anything>` therefore minted a fresh
            // epoch for **every stream in the account**, handed it to every
            // honest peer, and read everything they wrote next — while being
            // told, correctly, that it had revoked nothing.
            //
            // Skipping it costs no disclosure. No caller reads
            // `unrotated_streams` on this path: the CLI returns before it
            // prints the rotation report, and `DeviceListSection` replaces the
            // whole report rather than appending to it. `unrotatable` stays
            // empty, which is the truth — nothing was left unrotated because
            // nothing was rotated.
            //
            // Where the target was *already* revoked by a surviving row,
            // `effective` is true and this still runs. That case is a
            // re-revocation of a device the account has cut, and rotating
            // again is harmless and keeps the command's meaning uniform.
            //
            // `Command::RotateStreamKey` reaches the same mint-and-distribute
            // chain one stream at a time, and is gated on this device's own
            // standing in `rotate_stream_key` — there as a plain refusal,
            // because it has no recovery path to keep open.
            if effective {
                let set = self.keychain.rotation_set(tx)?;
                unrotatable = set.unrotatable;
                for stream_id in set.streams {
                    let (epoch, key) =
                        self.keychain
                            .mint_epoch(tx, &stream_id, self.rng.as_ref(), now_ms)?;
                    self.emit_key_envelopes(
                        tx,
                        &stream_id,
                        epoch,
                        &key,
                        now_ms,
                        Some(&seal_under),
                    )?;
                }
            }

            // Read last, so it reflects the fold this transaction just ran.
            //
            // The set is the two revocation tables disagreeing: bounded, and
            // not currently called revoked. `refold_device_revocations` warns
            // `core.device.revocation_unwound` at the moment it produces one;
            // this is the same fact read out of the tables it wrote, so there
            // is one derivation and not two, and it survives the restart a log
            // line does not. It is the standing set rather than this command's
            // delta on purpose — see `CommandResult::revocation_unwound`.
            {
                let mut stmt = tx.prepare(
                    "SELECT b.device_id FROM device_read_bounds b
                     WHERE NOT EXISTS (
                         SELECT 1 FROM device_revocations r
                         WHERE r.device_id = b.device_id
                     )
                     ORDER BY b.device_id",
                )?;
                unwound = stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<Vec<u8>>>>()?;
            }
            Ok(())
        })
        .map_err(|e| match e {
            sunrise_storage::DbError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
                EngineError::NotFound(format!("device {}", hex_short(&revoked)))
            }
            other => EngineError::Storage(other),
        })?;

        // 4. Rotate the **identity**, excluding the device just revoked.
        //
        //    This is what makes the revocation stick, and it is a second
        //    transaction on purpose. The first one has to commit before this
        //    one runs: `rotate_identity` builds its roster from the recipient
        //    rule — current, unrevoked, not excluded — and the revocation
        //    register it reads is written above. Building both in one
        //    transaction would work, but it would also mean a rotation whose
        //    roster depends on uncommitted state it cannot re-read after a
        //    rollback.
        //
        //    `keep_recovery_code: true` is the request; `rotate_identity`
        //    overrides it when the device being revoked is the one holding
        //    `ID_D_priv`, because a carry share sealed to that key would hand
        //    the successor straight to the device being excluded.
        //
        //    # Why this is best-effort now
        //
        //    Signing a transition needs `ID_S_priv`, and since #105 closed the
        //    pairing hole only the device that created the account holds it. A
        //    revocation run from a paired device therefore cannot rotate, and
        //    refusing the whole command would be worse than not rotating: the
        //    device the user is trying to revoke is often the one they have
        //    lost, and the creator may be the one they lost.
        //
        //    What is given up is bounded, because rotation is no longer what
        //    stops a revoked device certifying itself back in — *not holding
        //    `ID_S_priv`* is, and a revoked device admitted by pairing never
        //    held it. Everything above still runs: the register is written, the
        //    cut is recorded, every stream rotates and the revoked device is
        //    excluded from every new epoch.
        //
        //    The exception is revoking the **creator**, which is the one device
        //    that does hold `ID_S_priv` and so the one case where the identity
        //    genuinely needs to move. A paired device cannot do it, and the
        //    warning says so rather than letting the user believe otherwise.
        //    `docs/03-crypto/key-rotation.md` §Revocation is the procedure.
        //
        //    # And skipped entirely when the fold discarded the op
        //
        //    On the same predicate and for the same reason as the stream
        //    rotation above. `rotate_identity` mints a successor `ID_S_priv`
        //    and seals an HPKE share of it to each survivor's `D_D_pub`; run
        //    from a device the account has itself revoked, over a revocation
        //    the account does not believe, it hands that device a fresh
        //    account identity it then distributes. There is also nothing for
        //    it to exclude — the target is not revoked — so the rotation would
        //    move the whole account's identity to achieve nothing.
        //
        //    `core.identity.rotation_unavailable` is deliberately *not*
        //    emitted here. It means "the revocation cut every future key but
        //    could not move the identity", and on this path there was no
        //    revocation to cut anything; the fact a caller needs is
        //    `CommandResult::revocation_gated`, which says it exactly.
        if !effective {
            // Nothing to rotate away from, and nothing to say about it.
        } else if self.keychain.can_rotate_identity() {
            let rotation = self.rotate_identity(db, Some(revoked), true)?;
            if !rotation.carried_recovery_code {
                tracing::warn!(
                    ev = "core.identity.recovery_code_invalidated",
                    subject_h = hex_short(&revoked),
                    head_h = hex_short(&rotation.to_identity_id),
                    "the revoked device held the account's recovery key, so the successor \
                     could not be carried forward under it; the user needs a new recovery code"
                );
            }
        } else {
            tracing::warn!(
                ev = "core.identity.rotation_unavailable",
                subject_h = hex_short(&revoked),
                "this device was admitted by pairing and holds no account signing key, so the \
                 revocation cut every future key without rotating the identity; a revoked \
                 device that was itself paired cannot certify itself back in either way, but \
                 revoking the device that created the account needs to be done from that device"
            );
        }

        // The disclosure. A caller that printed "revoked" over a non-empty
        // list would be telling a user their stolen laptop had been cut off
        // from streams it can still read, which is the same disclosure failure
        // issue #160 fixed for the relay half.
        let unrotated_streams: Vec<String> = unrotatable.iter().map(|b| hex_bytes(b)).collect();
        if !unrotated_streams.is_empty() {
            tracing::warn!(
                ev = "core.device.revoke_incomplete",
                subject_h = hex_short(&revoked),
                n_streams = unrotated_streams.len(),
                "a revocation could not rotate every stream: these rows name a stream id \
                 that is not 16 bytes, so there was no epoch to mint, and the revoked \
                 device still holds whatever key it was last given for them"
            );
        }

        // The unwind disclosure. Same rule as `unrotated_streams` above and as
        // `revocation_gated`: a fact that changes what the account believes
        // reaches the caller rather than stopping at an operator's NDJSON
        // (`docs/10-cross-cutting/log-events.md`).
        //
        // **No event is emitted here, and that is deliberate.**
        // `core.device.revocation_unwound` already names this fact, once per
        // device, at the moment `refold_device_revocations` produces it. A
        // second emission under the same name with a different field shape
        // would make the catalogue's one row describe two things, and a third
        // event name for the same fact is what
        // `docs/10-cross-cutting/logging.md` §3 asks nobody to add. What was
        // missing was never a log line; it was that the fact reached only an
        // operator's NDJSON.
        let revocation_unwound: Vec<String> = unwound.iter().map(|b| hex_bytes(b)).collect();

        Ok(CommandResult::new(device_id, None, op_id, seq)
            .with_unrotated_streams(unrotated_streams)
            .with_revocation_gated(!effective)
            .with_revocation_unwound(revocation_unwound))
    }
    /// Whether `device_id` is revoked: **is there a row**, and nothing else.
    ///
    /// **The read half of revocation no longer calls this.** It did, from
    /// [`Self::backfill_key_envelopes`] and as an inlined anti-join in
    /// [`Self::emit_key_envelopes`]; since migration 0028 both read
    /// `device_read_bounds` through [`Self::is_read_bounded`], because a bound
    /// a re-fold can release is not a bound. The module doc above lists what
    /// still asks *this* question, which is the convergent one: the device
    /// list, which reads the register directly, and
    /// `CommandResult::revocation_gated` in [`Self::revoke_device`]. The
    /// *register* it reads is no longer written incrementally: see
    /// [`Self::refold_device_revocations`].
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

    /// Whether `device_id` is **read-bounded**: has any revocation this replica
    /// ever believed named it?
    ///
    /// The other question, and the one the four key-distribution sites ask.
    /// [`Self::is_revoked`] answers "is this device currently called revoked?",
    /// which is derived, has to converge, and can therefore go from true back
    /// to false when the fold learns a revocation's author was itself revoked.
    /// This one answers "may this device be given a key?", and it only ever
    /// goes from false to true for a device with a cert here: written by the
    /// fold's `INSERT OR IGNORE` and 0028's seed, and deleted only by
    /// [`Self::release_orphan_read_bounds`], for certless ids no row names.
    ///
    /// Keeping them apart is what makes the threat model's A3 sentence true.
    /// While one table served both, a routine unwind — retire the old laptop
    /// from the desktop, months later retire the desktop from the phone — put
    /// the laptop back in `emit_key_envelopes`' recipient set for every epoch
    /// minted from then on, and let one `DeviceCertPublish` from it pull every
    /// held epoch of every stream back out of
    /// [`Self::backfill_key_envelopes`].
    ///
    /// Presence and nothing else, for the reason [`Self::is_revoked`] gives at
    /// length: the only reading comparable with another device's cut is this
    /// device's own HLC, and a fresh [`crate::config::MonotonicHlc`] reads zero
    /// after any restart. `first_bound_at_ms` gates nothing and, as things
    /// stand, **nothing reads it**: the read-bounded signal both device lists
    /// render is the presence of the row. The column is kept because the seed
    /// already fills it and re-adding it later would not recover the value for
    /// the vaults that had it; `migrations/0028_device_read_bounds.sql` says so
    /// at the schema.
    ///
    /// Like the register, this is the *local* half: a replica that has not yet
    /// applied the `device_revoke` op has no row to read and will seal the
    /// device a new epoch.
    ///
    /// **And unlike the register, it does not converge.** The only move this
    /// table makes is to bound one more device, which makes it monotone along
    /// *this replica's own arrival order* — not a function of the op set. Two
    /// replicas that applied the same ops in different orders can settle on
    /// permanently different bounds, because a row gated at every fold one of
    /// them runs never reaches its `register` and so never reaches its bound.
    /// [`Self::refold_device_revocations`] works the counterexample through;
    /// [#282](https://github.com/justin13888/Sunrise/issues/282) is the open
    /// question of what a converging derivation would be. So a caller may read
    /// this as "has *this* replica bounded the device", and may not read it as
    /// "has the account".
    pub(super) fn is_read_bounded(
        &self,
        conn: &rusqlite::Connection,
        device_id: &[u8; 16],
    ) -> rusqlite::Result<bool> {
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM device_read_bounds WHERE device_id = ?",
                params![&device_id[..]],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Recompute `device_revocations` from every `device_revoke` op this vault
    /// holds, and return the ops the fold **skipped** because the account had
    /// already revoked the device that wrote them.
    ///
    /// # Why the register is a fold and not an upsert
    ///
    /// Until ADR-0041 the arm applying a `device_revoke` refused exactly one
    /// thing: an op naming its own sender. So a device the account had already
    /// revoked could still revoke every *other* device in it — its cert is on
    /// the chain, it holds the vault-meta key for the epoch it was cut at, and
    /// peers keep old epoch keys, so the op verifies, decrypts and applies
    /// everywhere. Revocation has no inverse, so the damage was permanent on
    /// every replica: an expelled laptop could expel the account
    /// ([#82](https://github.com/justin13888/Sunrise/issues/82)).
    ///
    /// Refusing the op *where it arrives* would have made the register depend
    /// on delivery order — a replica that applied it before learning its sender
    /// was revoked keeps the row, one that met them the other way round does
    /// not — which is exactly the divergence
    /// [ADR-0034](../../../../docs/11-adr/0034-revocation-bounds-reads-not-writes.md)
    /// corollary 3 forbids peer-side enforcement from reintroducing.
    ///
    /// So the op is stored unconditionally and the register is derived. The
    /// fold walks `device_revoke_ops` in one canonical total order —
    /// `(op_hlc_ms, op_hlc_logical, sender, revoked_device_id)`, the order the
    /// register's own LWW comparator already used, extended by the one column
    /// that makes it total. The target is in the order because it is in the
    /// table's primary key: the sender picks its own HLC, so two
    /// `device_revoke` ops from one member at one stamp naming two targets are
    /// two distinct rows, and without the fourth column the walk would leave
    /// their relative order to SQLite. See
    /// `crates/sunrise-storage/migrations/0027_device_revoke_ops.sql` for why
    /// the op id is not needed and the target is.
    ///
    /// Before the walk it builds `revokers_all` — who has revoked whom, over
    /// the **whole ledger** — discounts from it every revoker the ledger
    /// itself shows expelled by a third party, and skips a row whose sender
    /// survives in what is left. The order above still decides the register,
    /// because a later row overwrites an earlier one; it no longer decides the
    /// gate. Every replica holding the same ops reaches the same register,
    /// whatever order the ops arrived in.
    ///
    /// # Why the gate reads the whole ledger and not the prefix
    ///
    /// Because the sort key is the sender's to choose, and judging the sender
    /// against a prefix of it made the gate a number an attacker picks.
    /// [`Hlc`] bounds only the future — `MAX_DRIFT_MS` — and a stamp in the
    /// past is ordinary, which `crates/sunrise-core/src/engine/sync.rs` says
    /// outright. Nothing ties an op's HLC to that sender's own `seq`, to its
    /// meta epoch, or to any earlier stamp it sent, and the local path could
    /// not carry the `seq` even if the rule wanted it: `apply_control_op` runs
    /// before `ensure_stream_epoch` while `next_seq_tx` must run after it, for
    /// the reason [`Self::revoke_device`] gives. So a revoked device dated its
    /// `device_revoke` ops below its own cut, they sorted first, the prefix
    /// had not yet revoked their sender, and they landed — and iterating the
    /// remaining device ids revoked the whole account for the price of
    /// choosing a small integer. That is the outcome ADR-0041 exists to
    /// prevent and the one its §Alternatives (f) rejects a rival design for
    /// admitting.
    ///
    /// A set built from every row is not a number an attacker can move, and
    /// it closes the no-history case too, because `revokers_all[X]` holds X's
    /// own revoker by construction. The discount below is the one thing that
    /// takes an entry back out of that set, and X can never be the reason:
    /// discounting X's revoker needs a row whose sender is neither X nor that
    /// revoker, and a device authors only rows whose sender is itself. So the
    /// gate the walk applies is weaker than this set and still not one X can
    /// move. It reads no clock of any kind — not the
    /// op's and not this device's, the latter being the read
    /// [`Self::is_revoked`] documents at length as unsound, because a fresh
    /// [`crate::config::MonotonicHlc`] after a restart collapses it to a bare
    /// wall clock. It asks a set question instead: has anybody other than the
    /// device this row names revoked this row's sender? And it stays a pure
    /// function of the ledger's row set rather than of delivery order, which
    /// is what
    /// [ADR-0034](../../../../docs/11-adr/0034-revocation-bounds-reads-not-writes.md)
    /// corollary 3 requires; if anything more obviously so, because it no
    /// longer depends on where in the walk a row sits.
    ///
    /// # What this gives up: revocation here **is** retroactive
    ///
    /// A revoked device's revocations from *before* its own cut no longer
    /// stand. That is deliberate and it is the price of the paragraph above:
    /// "before its own cut" is a number the sender picks, and nothing in the
    /// ledger distinguishes an honestly-earlier revocation from a back-dated
    /// one. Preserving the distinction needs evidence the ledger does not
    /// carry, and a rule that preserved it only for senders with history is
    /// evaded by a sender that files none.
    ///
    /// Revocation is not retroactive anywhere else in this module — a cert
    /// issued under a superseded identity still identifies its device — and
    /// this family is the exception because its effect can be re-derived,
    /// which is the same scoping argument ADR-0041 makes for gating control
    /// ops and not entity ops. The consequence is that
    /// `core.device.revocation_unwound` below is routinely reachable rather
    /// than exotic: a revocation this replica already believed stops being
    /// believed when it learns its author had been revoked. That is the
    /// correct signal and it is why it is said out loud.
    ///
    /// # The mutual exception's cost: a permanent third-party lockout
    ///
    /// Stated here rather than left to be deduced from the gate, because it is
    /// a behaviour a user can reach and not an implementation detail.
    ///
    /// After two devices have revoked each other, each one's revoker set holds
    /// exactly one entry and it is the other. So each is forgiven for revoking
    /// the other — that is the exception, and it is what makes the pair
    /// converge on both revocations — and gated for revoking **anybody else**.
    /// It reaches the honest device of the pair too, and it lasts until a
    /// third current device revokes the half it believes compromised. That
    /// device's row gives the compromised half a second revoker, so its
    /// revocation of the honest half is gated. The same row discounts the
    /// compromised half out of the honest half's revoker set, so the honest
    /// half is current and ungated again. Pinned by
    /// `a_third_current_device_settles_which_half_of_a_mutual_pair_the_account_meant`.
    /// Until then, one op from a device the account has already expelled costs
    /// the device that expelled it the ability to revoke third parties. In a
    /// two-device account there is no third device, so it is for good.
    ///
    /// **What it does not cost, stated as the bound rather than as a hope.**
    /// This paragraph claimed that every other current device in the account
    /// still revokes whoever it likes, and for one revision of this function
    /// that was false: the revoker map was built from every row while only the
    /// walk judged one, so a revoked device reached *every* current device
    /// with one gated op each and gated the whole account out of revoking
    /// anything. The discount pass is what makes the claim true again, and
    /// what it is now true of is this:
    ///
    /// A revoked device X can enter the revoker set of a device V only when V
    /// is the **only** device that has revoked X. That follows from the
    /// discount and not from good behaviour: X is revoked, so some device O
    /// revoked it; X survives in V's set only when no row revokes X from a
    /// sender other than V; so O is V, and O is X's sole revoker. Two
    /// consequences a reader can rely on. A device X merely *named* is
    /// untouched, because it did not revoke X. And a second device that also
    /// revoked X is untouched, because each of the two is then a revoker of X
    /// other than the other. So the cost is the mutual pair and nothing
    /// wider — the two devices in the relationship, and no third.
    ///
    /// The rest of the remedy is unchanged: revocation is not gated on
    /// `ID_S_priv` anywhere, so identity rotation and pairing sponsorship are
    /// untouched, and any current device outside the pair still revokes
    /// whoever it likes. The lockout is total only in a two-device account,
    /// where there is no third — and there the survivor has nothing left to
    /// revoke but itself, which [`Self::revoke_device`] refuses anyway.
    ///
    /// # What the discount gives up: rehabilitation by a third party
    ///
    /// The discount asks its question of the **ledger** — has anybody other
    /// than V expelled S? — and not of the register, because asking the
    /// register is the wider form ADR-0041 §Alternatives (h) prices, which
    /// reopens the bypass above with the arrow reversed. The price of asking
    /// the ledger is a named consequence rather than a surprise, and its
    /// condition is exactly that question answered yes.
    ///
    /// *Shape one, and only half of it is new.* O revokes X; a third party P
    /// then revokes O. X was already off the revoked list, because a revoked
    /// device's revocations are unwound whatever date they carry — that is the
    /// retroactivity above and it predates the discount. What the discount
    /// adds is that X is no longer *gated* either, so X revokes third parties
    /// again. Pinned by
    /// `the_discount_rehabilitates_a_device_whose_sole_revoker_a_third_party_revokes`.
    ///
    /// **P does not have to be a bystander, and this is the shape a threat
    /// model has to carry.** One attacker holding two devices the account
    /// revoked together reaches it with a single op: O revoked X1 and X2, X1
    /// revokes O, the mutual exception lands that, O goes out, and O going out
    /// both unwinds its revocation of X2 (decision 1, predating the discount)
    /// and discounts O out of X2's set (the discount). X2 then revokes the
    /// rest of the account. The remedy is the mutual pair's and no better: a
    /// device X2 reaches revokes it back and is left revoked itself. Pinned by
    /// `the_discount_lets_one_of_two_devices_revoked_together_ungate_the_other`.
    ///
    /// *Shape two, which is the hole.* Extend that by one link — O revokes X,
    /// P revokes O, Q revokes P — and Q's row gates P's, so O's revocation of
    /// X stands and **X is on the revoked list while being ungated**, which is
    /// the pair of facts this gate exists to keep apart. It costs three
    /// revocations arranged in a chain, and X can author none of the two that
    /// matter: a device authors only rows whose sender is itself, and every
    /// discount of S from V's set needs a row from a sender that is not V, so
    /// X can never discount anything out of its own set — the rows that
    /// rehabilitate it are written by other devices, whether honest ones or a
    /// second device the same attacker holds. Pinned by
    /// `the_discount_leaves_a_revoked_device_revoking_when_a_chain_revokes_its_revoker`.
    ///
    /// What closes it is the same thing that closes the lockout, and it
    /// already exists: a current device revoking X. That device is a revoker
    /// nobody discounts, so X is gated again. An un-revoke op is not needed to
    /// say which reading of a revoked revoker the account meant, and a
    /// third-party one is rejected as new authority. The one inverse ADR-0056
    /// (`docs/11-adr/0056-a-revocation-is-withdrawn-only-by-its-author.md`)
    /// takes is a withdrawal by a revocation's own author, and it is not built
    /// yet ([#383](https://github.com/justin13888/Sunrise/issues/383)).
    ///
    /// It is recorded rather than repaired because no ledger-only rule can do
    /// better. After a mutual revocation the two devices are symmetric in the
    /// ledger: nothing distinguishes the honest one from the compromised one,
    /// so ungating both would hand an attacker the account. Exempting from a
    /// device's revoker set any revoker the final register revokes reopens the
    /// bypass above, with the arrow reversed. What tells the two apart is a
    /// row the ledger does not yet hold, and a third current device writes it.
    /// `a_mutual_pair_locks_both_devices_out_of_third_party_revocation` pins
    /// the behaviour so it stays deliberate.
    ///
    /// # Why `sender` is the row's author, and where that is enforced
    ///
    /// Everything above that turns on `sender != v` — the discount's rule, the
    /// walk's exception, and the twice-stated "a device authors only rows whose
    /// sender is itself" that bounds both residuals — needs `sender` to be the
    /// *authenticated* device and not a value the op chose. Neither this
    /// function nor `apply_device_revoke` checks that, and neither could: the
    /// column is written from `env.device_id`
    /// (`crates/sunrise-core/src/engine/sync.rs:348`), and what binds that id to
    /// a key lives in the sync path and in `sunrise-crypto`. Cited rather than
    /// assumed, because it is the load-bearing bound of this whole function and
    /// it is enforced in another module:
    ///
    /// - `crates/sunrise-core/src/engine/sync.rs:247-253` resolves the signing
    ///   key **by** `env.device_id` — the cert stored under that id, or, for a
    ///   device publishing its first cert,
    ///   [`Self::self_authenticating_signer`].
    /// - `crates/sunrise-core/src/engine/sync.rs:257` verifies the envelope
    ///   under that key before the op is decrypted or applied, and
    ///   `crates/sunrise-crypto/src/op_envelope.rs:491-497` is the check
    ///   itself: an Ed25519 verify over the envelope's own signed bytes.
    /// - `crates/sunrise-core/src/engine/sync.rs:411` closes the
    ///   self-authenticating half, refusing a published cert whose
    ///   `body.device_id` is not `env.device_id` — so a device cannot present
    ///   another device's cert and author rows under its id.
    ///
    /// A sender that could forge the column would be gated by nobody and could
    /// discount anything out of anybody's set, which is every bound above at
    /// once.
    ///
    /// # Recoverability: a cut correction does **not** un-skip a revocation
    ///
    /// A skipped op's pair stays (unless its sender passes the per-sender cap),
    /// so there is nothing to re-request — but the skip is not undone by a cut
    /// either, and the reason is the paragraphs above: **the gate reads no cut
    /// of any kind**. `revokers_all`, the discount and the walk's condition are
    /// built from `(sender, revoked)` pairs and nothing else; the HLC decides
    /// only which row wins the register. Correcting a cut appends a second
    /// revocation of the same sender by the same party, which changes that
    /// winner and changes nothing about who has revoked whom — so the gate
    /// answers the same question the same way, whether the correction is dated
    /// after the op it would rescue or before it.
    /// `a_cut_correction_does_not_re_fold_a_skipped_revocation` pins it.
    ///
    /// What *does* un-skip a row is the one thing that empties its sender's
    /// revoker set: somebody revoking that sender's revoker, which is the
    /// discount above. The row's own sender can never author it, for the reason
    /// §"Shape two" gives — a device authors only rows whose sender is itself.
    ///
    /// So #82's two honest options — re-request the op, or accept the loss and
    /// say so where a user can see it — are answered with the second, and the
    /// remedy is the one the rest of this family already has: make the
    /// revocation again from a device the account still trusts.
    /// `core.device.revoke_refused` with `reason = "revoked_sender"` is where an
    /// operator sees that one is needed. ADR-0041 §"What a user sees" item 3
    /// records the same thing for a reader who starts from the decision.
    fn refold_device_revocations(
        &self,
        tx: &Transaction<'_>,
    ) -> rusqlite::Result<Vec<SkippedRevoke>> {
        let mut stmt = tx.prepare(
            "SELECT op_hlc_ms, op_hlc_logical, sender, revoked_device_id, reason,
                    recorded_at_ms
             FROM device_revoke_ops
             ORDER BY op_hlc_ms ASC, op_hlc_logical ASC, sender ASC,
                      revoked_device_id ASC",
        )?;
        let rows: Vec<RevokeLedgerRow> = stmt
            .query_map([], |r| {
                Ok(RevokeLedgerRow {
                    hlc_ms: r.get(0)?,
                    hlc_logical: r.get(1)?,
                    sender: r.get(2)?,
                    revoked: r.get(3)?,
                    reason: r.get(4)?,
                    recorded_at_ms: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        // Who has revoked whom, over the **whole ledger** — every revoker and
        // not only the one currently winning the register, and every row and
        // not only the ones sorting below the row being judged.
        //
        // Reading the whole ledger rather than the prefix is what closes the
        // bypass. The sort key is entirely the sender's to choose: `Hlc`
        // bounds only the future (`MAX_DRIFT_MS`), a stamp in the past is
        // ordinary and `sync.rs` says so, and nothing ties an op's HLC to that
        // sender's own `seq` or to any earlier stamp it sent. Against a prefix
        // a revoked device simply dated its `device_revoke` ops below its own
        // cut: they sorted first, the prefix had not yet revoked their sender,
        // and they landed. Iterating the other device ids then revoked the
        // whole account for the price of choosing a small integer — the exact
        // outcome ADR-0041 exists to prevent, and the one its §Alternatives
        // (f) rejects a rival design for admitting.
        //
        // A set built from every row is not a number an attacker can move.
        // It is also still a pure function of the ledger's row set and not of
        // delivery order, which is what ADR-0034 corollary 3 requires; if
        // anything it is more obviously so, because it no longer depends on
        // where in the walk a row sits.
        //
        // This is the map the discount below reads and the walk does not. The
        // walk reads what the discount leaves.
        //
        // `s != v` here because a device cannot revoke itself, so it must not
        // enter its own revoker set and gate everything it goes on to write.
        //
        // **Defensive against a future writer, not against rows that exist.**
        // No self-naming row can reach `device_revoke_ops` at HEAD — see the
        // walk below, which states both sources — so nothing reaches this
        // `continue` today. It is kept because a pair is dropped only when its
        // own sender passes the cap: a writer admitting one would put the
        // device it named beyond revoking anything, everywhere, for good.
        //
        // The walk below holds the same condition and it is **not** a
        // duplicate: this one bounds the *gate*, that one bounds the
        // *register*. See it for what each prevents.
        let mut revokers_all: std::collections::BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>> =
            std::collections::BTreeMap::new();
        for row in &rows {
            if row.sender == row.revoked {
                continue;
            }
            revokers_all
                .entry(row.revoked.clone())
                .or_default()
                .insert(row.sender.clone());
        }

        // **The discount, and why there is a second pass at all.**
        //
        // The map above is built from every row, and the walk below is the
        // only thing that judges one — so a row the walk *gates* has already
        // seated its sender in its target's set. That was a hole the width of
        // the account. A device the account had expelled names each remaining
        // device in one ordinary op apiece; every op is correctly gated and
        // revokes nobody; and every one of them leaves the expelled device
        // sitting in its target's revoker set, which gates that target out of
        // revoking anything, permanently and on every replica. No crafted
        // stamp and no back-dating: N ordinary ops, and the account can never
        // revoke a stolen device again.
        //
        // The rule that closes it: **discount `s` from `v`'s set when the
        // ledger holds a row revoking `s` whose sender is not `v`**. Read it
        // as "`s`'s claim on `v` is worth nothing once somebody other than
        // `v` has expelled `s`" — the same question the gate asks, asked
        // about the claim rather than about the op that carried it.
        //
        // `sender != v` is the whole of why this does not reopen §Decision
        // 1's hole with the arrow reversed, which is the failure ADR-0041
        // §Alternatives (h) prices for the *wider* form that discounts any
        // revoker the register revokes. Under the wider form: X, revoked by
        // O, emits `device_revoke(O)`; the mutual exception lands it; the
        // register revokes O; X's set empties; X's third-party rows land.
        // Here it cannot. X's only revoker is O, and no row revokes O from a
        // sender other than X, so nothing is discounted and X stays gated.
        // The mutual pair's lockout is preserved exactly, which is also why
        // this fires in none of the cases §"What a user sees" item 4 is
        // about.
        //
        // All of which holds only because `sender` is the device the sync path
        // authenticated and not a field the op filled in. This function's doc
        // §"Why `sender` is the row's author" cites where that is enforced —
        // in `crates/sunrise-core/src/engine/sync.rs` and
        // `crates/sunrise-crypto/src/op_envelope.rs`, with lines.
        //
        // Those citations are resolvable pointers, and not assertions about the
        // checks. `.github/scripts/citation-gate.py`'s `classify` fails a
        // `path:line` on two conditions only: the path names no file git
        // tracks, or the file has fewer lines than the citation's last. A check
        // that *moves within* its file leaves the citation green while it
        // points somewhere else, and `sync.rs` is over a thousand lines — so
        // what the gate buys here is that a reader always lands in the right
        // file, and that deleting or shrinking either one goes red. Verifying
        // that a citation names a claimed symbol is
        // [#249](https://github.com/justin13888/Sunrise/issues/249).
        //
        // It reads `revokers_all` and never itself, so it is a second pass
        // over a frozen map and not a fixpoint — no entry's fate depends on
        // another entry's fate, and the fixpoint forms are what ADR-0041
        // §Alternatives declines as non-monotone. It therefore stays a pure
        // function of the ledger's row set, with no dependence on the walk or
        // on delivery order, which is what
        // [ADR-0034](../../../../docs/11-adr/0034-revocation-bounds-reads-not-writes.md)
        // corollary 3 requires.
        //
        // What it does **not** close is recorded, with its condition, in
        // ADR-0041 §"What a user sees" item 4 and pinned by
        // `the_discount_rehabilitates_a_device_whose_sole_revoker_a_third_party_revokes`
        // and
        // `the_discount_leaves_a_revoked_device_revoking_when_a_chain_revokes_its_revoker`.
        let revokers: std::collections::BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>> = revokers_all
            .iter()
            .map(|(revoked, senders)| {
                let kept = senders
                    .iter()
                    .filter(|sender| {
                        !revokers_all
                            .get(*sender)
                            .is_some_and(|who| who.iter().any(|other| other != revoked))
                    })
                    .cloned()
                    .collect();
                (revoked.clone(), kept)
            })
            .collect();

        // Ascending, so a later row simply overwrites an earlier one: that is
        // the LWW register, written as the fold it always was.
        let mut register: std::collections::BTreeMap<Vec<u8>, RevokeLedgerRow> =
            std::collections::BTreeMap::new();
        let mut skipped = Vec::new();
        for row in rows {
            // A device must not move its own cut, and this is where that is
            // enforced for the **register**.
            //
            // Not a second copy of the check in the map pass above, though it
            // reads like one: the two prevent different things about the same
            // hypothetical row. That one keeps a self-naming row from seating
            // its sender in its own revoker set, which would gate everything
            // that device ever wrote. This one keeps it out of the register,
            // and therefore out of `device_read_bounds` — a row that landed
            // here would revoke a device on its own say-so and cut its keys
            // permanently, since the bound is taken from `register` and never
            // released. Deleting either leaves the other's failure open.
            //
            // Both are defensive against a future writer rather than against
            // rows that exist, and neither is reachable at HEAD: the live path
            // refuses a self-naming op before the insert
            // (`apply_device_revoke` returns early), and 0027's seed reads
            // `device_revocations`, which the pre-ADR-0041 arm also refused to
            // write for one. A pair is dropped only by its own sender's cap, so
            // the cost of a writer that admitted one is not recoverable — which is
            // why the checks are kept and said to be unreachable rather than
            // quietly relied on.
            if row.sender == row.revoked {
                continue;
            }
            // **The gate, and its one exception.**
            //
            // A row is skipped when the ledger anywhere revokes its sender,
            // discounting revokers the discount pass above dropped — but not
            // when the *only* party to have revoked it is the very
            // device this row is about. Without that exception two devices
            // revoking each other would stop converging on both revocations:
            // whichever op sorted first would silence the other, and an HLC
            // sorts first by being dated earlier, which is free. A stolen
            // laptop would simply back-date its revocation of the owner's Mac
            // and the Mac's answer would never land — an easier takeover than
            // the one this fold exists to stop.
            //
            // So "B says A is out" is not on its own a reason to disbelieve "A
            // says B is out"; the two claims are symmetric and the safe
            // resolution is the one the engine already had, which is that both
            // devices end up revoked. It becomes a reason the moment A reaches
            // for a *third* party, or the moment anyone other than B has also
            // revoked A. Both are this condition.
            let gated = revokers
                .get(&row.sender)
                .is_some_and(|who| who.iter().any(|r| *r != row.revoked));
            if gated {
                skipped.push(SkippedRevoke {
                    sender: row.sender,
                    hlc_ms: row.hlc_ms,
                    hlc_logical: row.hlc_logical,
                });
                continue;
            }
            register.insert(row.revoked.clone(), row);
        }

        // A re-fold can *remove* a revocation: a revocation of S arriving now
        // can skip rows S wrote later, and a device S had revoked becomes
        // current again. That is the fold being a pure function of the op set
        // rather than a ratchet, and it is the one outcome of this design a
        // user could be surprised by, so it is said out loud rather than
        // deduced from a device list that quietly changed. The remedy is to
        // revoke that device again from a device the account still trusts.
        {
            let mut before = tx.prepare("SELECT device_id FROM device_revocations")?;
            let held: Vec<Vec<u8>> = before
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for device_id in held {
                if !register.contains_key(&device_id) {
                    tracing::warn!(
                        ev = "core.device.revocation_unwound",
                        // The same four-byte prefix `hex_short` renders, over
                        // a blob whose length the schema does not fix.
                        subject_h = hex_bytes(&device_id[..device_id.len().min(4)]),
                        "a revocation is no longer believed: the device that made it \
                         had itself been revoked first"
                    );
                }
            }
        }
        // **The read bound is taken here, and it is never given back.**
        //
        // Everything above is the register, which is derived and therefore not
        // monotone: the `DELETE` below rebuilds it, and a row can be absent
        // from the rebuild. That is correct for the question the register
        // answers — "is this device currently called revoked?" — which has to
        // converge on the op set and not on delivery order.
        //
        // It is not correct for the other question `device_revocations` used to
        // be asked, which is whether a device is **read-bounded**. That one
        // gates all four key-distribution sites (`emit_key_envelopes`'
        // anti-join, `backfill_key_envelopes`' early return, the survivor
        // roster `rotate_identity` builds, and the readmission signal in the
        // `DeviceCertPublish` arm), and a bound that a later op can release is
        // not a bound: an unwound device became a full recipient again for
        // every epoch the vault minted afterwards, and one `DeviceCertPublish`
        // from it drove `backfill_key_envelopes` to hand back every held epoch
        // of every stream.
        //
        // So the bound is its own table and this is its only writer:
        // `INSERT OR IGNORE` over the register this fold just computed, run
        // **before** the `DELETE`, so no row can pass through a window where it
        // is in neither. Only `release_orphan_read_bounds` deletes, and never a
        // certed device — `migrations/0028_device_read_bounds.sql` is why a ratchet
        // rather than a second fold, and why making the register itself the
        // ratchet was rejected.
        //
        // **The cost, and it is larger than a floor: the bound does not
        // converge.** What this `INSERT OR IGNORE` buys is monotonicity along
        // *this replica's own arrival order*, and nothing more. The bound is a
        // union of the registers this replica happened to compute, so a row
        // gated at every fold this replica runs never enters `register` and
        // therefore never enters the bound — permanently, however far the ops
        // propagate.
        //
        // The counterexample is 0028's own motivating scenario. A retires
        // laptop C at `h1`; months later B retires A at `h2 > h1`.
        //
        // * A replica applying `A -> C` first folds it to the register `{C}`
        //   and bounds C. The second op gates `A -> C`, rebuilding the register
        //   as `{A}`, so the bound ends `{C, A}`.
        // * A replica applying `B -> A` first folds it to `{A}` and bounds A.
        //   Its second fold already gates `A -> C`, so `register` is `{A}`
        //   again and this insert is a no-op. **The bound ends `{A}`, and C is
        //   never bounded on that replica at all.**
        //
        // Identical op sets, identical registers, different fixed points — and
        // both replicas compute `{A}` on every later fold, so no amount of
        // further propagation repairs the second. It seals C every epoch it
        // mints, and one `DeviceCertPublish` from C drives
        // [`Self::backfill_key_envelopes`] to hand back every held epoch of
        // every stream: the failure this split exists to close, surviving at
        // the new site for a replica that met the ops the other way round.
        // Nor is it adversarial — `PairingPayload` carries no revocation
        // state, so a device that pairs today starts empty and learns the ops
        // in whatever order the relay has them.
        //
        // What *is* closed here is the whole of what one replica can observe:
        // once this replica has bounded a device, no later fold gives the bound
        // back, so an unwind can no longer readmit it to a recipient set. The
        // cross-replica half needs the bound to be a function of the op set,
        // which is a derivation this table does not have and
        // [#282](https://github.com/justin13888/Sunrise/issues/282) is where it
        // belongs — with the two questions it turns on: whether the bound is a
        // property of the account or of a replica's history, and whether
        // `PairingPayload` should carry it.
        //
        // A device the discount pass rehabilitates reads `current` in the
        // device list while receiving no keys; that asymmetry is real, and
        // `DeviceRow::read_bounded` is what puts it on screen instead of
        // leaving a user to infer it.
        {
            let mut bound = tx.prepare(
                "INSERT OR IGNORE INTO device_read_bounds (device_id, first_bound_at_ms)
                 VALUES (?1, ?2)",
            )?;
            for (device_id, row) in &register {
                bound.execute(params![device_id, row.recorded_at_ms])?;
            }
        }
        tx.execute("DELETE FROM device_revocations", [])?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO device_revocations
                 (device_id, cut_ms, cut_logical, revoked_by, reason, recorded_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (device_id, row) in &register {
                insert.execute(params![
                    device_id,
                    row.hlc_ms,
                    row.hlc_logical,
                    row.sender,
                    row.reason,
                    row.recorded_at_ms,
                ])?;
            }
        }
        Ok(skipped)
    }

    /// Apply one `device_revoke`, from a peer or from this device's own
    /// [`Self::revoke_device`].
    ///
    /// Split out of `apply_control_op`'s match so that the whole of what
    /// decides the register lives beside the fold that recomputes it.
    pub(super) fn apply_device_revoke(
        &self,
        tx: &Transaction<'_>,
        p: &DeviceRevokePayload,
        sender: &[u8; 16],
        hlc: Hlc,
        now_ms: u64,
    ) -> rusqlite::Result<Vec<([u8; 16], u32)>> {
        // The revocation row is an **LWW register** keyed on the op's own
        // `(hlc, device_id)` — the same rule ADR-0014 resolves every other
        // concurrent write in this engine with, rather than a bespoke one for
        // this family.
        //
        // The cut is that same HLC. There is no `effective_at` field to pick a
        // winner between, which is the point: see [`DeviceRevokePayload`] for
        // the two bounds that could not be made to hold on an emitter-chosen
        // one.
        //
        // Why LWW and not "earliest cut wins", which this briefly did:
        // `MIN` converges, but it is **irreversible**. A cut that lands too
        // far in the past — which a device with a slow clock produced through
        // the ordinary command, no crafted input needed — could never be
        // corrected, and it refuses its target's entire history on every
        // replica. Under LWW a later revocation supersedes an earlier one, so
        // a bad cut is fixed by revoking again from a healthy device.
        //
        // `logical` is in the key because an HLC is `(physical, logical)` and
        // comparing physical alone would drop the half that orders two ops
        // inside one millisecond. `revoked_by` breaks the remaining tie the
        // way `LwwStamp` does, so `revoke_reason` and `revoked_by` follow the
        // winning op instead of being order-dependent alongside a converged
        // timestamp.
        //
        // A device cannot revoke itself, and cannot move its own cut.
        // Register hygiene independent of any gate: a device that can
        // rewrite its own row can push its cut forward and undo a revocation
        // somebody else made of it, which is the one edit the register must
        // never accept from the party it is about. `Command::RevokeDevice`
        // refuses it locally for the separate reason that rotating every key
        // away from the only device holding them is not a recoverable state;
        // this is the remote half, and neither implies the other.
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
        // The op is **recorded whatever its sender's standing**, and the
        // register is recomputed from the record. Storing first and judging in
        // the fold is what keeps the judgement out of delivery order: see
        // [`Self::refold_device_revocations`] and ADR-0041.
        //
        // The ledger is its own table, and so is the register it folds to, so
        // a revocation naming a device this replica has never seen is durable
        // without inventing one. That case is ordinary: the cert travels in
        // the same stream with no ordering guarantee, and one parked in
        // `deferred_ops` at meta epoch k drains after a revocation absorbed at
        // k+1.
        //
        // Upserting into `devices` handled it and cost too much — every such
        // op minted a row there, so revocations of ids nobody knows became
        // phantom entries in the user's device list, and a phantom row also
        // satisfied `revoke_device`'s "is this device known" guard, which
        // would mint a fresh epoch for every stream in the account on the way
        // to revoking a ghost.
        //
        // `OR IGNORE`, because a re-delivered op must not move
        // `recorded_at_ms` and thereby change which row a tie resolves to. The
        // idempotence gate in `apply_remote_all` already stops most of that;
        // this is the same guarantee where the local path and a replay can
        // also reach.
        tx.execute(
            "INSERT OR IGNORE INTO device_revoke_ops
                     (op_hlc_ms, op_hlc_logical, sender, revoked_device_id, reason,
                      recorded_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                cut,
                logical,
                &sender[..],
                &p.revoked_device_id[..],
                p.reason_code.as_str(),
                i64::try_from(now_ms).unwrap_or(i64::MAX),
            ],
        )?;
        // Before the fold, unlike the pair compaction below, because this one
        // does change what the fold reads: see `cap_device_revoke_targets`.
        if Self::cap_device_revoke_targets(tx, sender, &p.revoked_device_id)? {
            tracing::warn!(
                ev = "core.device.revoke_refused",
                reason = "sender_over_cap",
                sender_h = hex_short(sender),
                subject_h = hex_short(&p.revoked_device_id),
                "a device has named more distinct targets than one sender may keep"
            );
        }
        let skipped = self.refold_device_revocations(tx)?;
        if skipped
            .iter()
            .any(|s| s.sender == sender[..] && s.hlc_ms == cut && s.hlc_logical == logical)
        {
            // Not a refusal to record — the row above is still there, and a
            // later revocation of this sender's revoker would re-fold it. What
            // was refused is the *effect*, and correcting the cut does not give
            // it back: the gate reads no cut. See
            // [`Self::refold_device_revocations`] §Recoverability.
            tracing::warn!(
                ev = "core.device.revoke_refused",
                reason = "revoked_sender",
                sender_h = hex_short(sender),
                subject_h = hex_short(&p.revoked_device_id),
                "a device the account had already revoked tried to revoke another"
            );
        }
        // After the fold and the skip check, so both read the op this call
        // stored even when it is one the compaction below removes.
        Self::compact_device_revoke_ops(tx)?;
        Self::release_orphan_read_bounds(tx)?;
        Ok(Vec::new())
    }

    /// Delete every ledger row the fold can never read the value of: all but
    /// the greatest-stamped row of each `(sender, revoked_device_id)` pair.
    ///
    /// # Why this changes no fold
    ///
    /// [`Self::refold_device_revocations`] reads a row in two places, and
    /// neither can tell a pair's lesser rows from its greatest:
    ///
    /// - **The gate** is built from `(sender, revoked)` pairs and nothing else
    ///   — `revokers_all`, the discount, and the walk's condition. Deleting a
    ///   row whose pair survives in another row leaves every one of them
    ///   unchanged, and so leaves every row gated or ungated as it was.
    /// - **The register** takes, per revoked device, the greatest ungated row in
    ///   the canonical order. Whether a row is gated depends only on its pair,
    ///   so a pair's rows are gated or ungated together; and within one pair
    ///   `sender` and `revoked_device_id` are equal, so the canonical order is
    ///   the HLC alone. If any row of a pair could win, the pair's greatest row
    ///   sorts after it and wins instead. A lesser row never wins.
    ///
    /// So the fold is the same function of the compacted ledger as of the full
    /// one, every register it produces is the same register, and
    /// `device_read_bounds` — which ratchets over those registers — takes the
    /// same rows it would have. That is the property ADR-0034 corollary 3 and
    /// ADR-0041 need from the ledger, and it is why this can delete from a
    /// table whose whole design is keeping ops: it deletes only rows the fold
    /// provably ignores.
    ///
    /// The compacted ledger is also itself a function of the op set rather
    /// than of arrival order: whichever order the ops came in, what remains is
    /// each pair's greatest stamp. A lesser op that arrives after its pair's
    /// greatest is stored, folded, reported on if gated, and removed here in
    /// the same transaction.
    ///
    /// # What it bounds, and what it does not
    ///
    /// The ledger holds at most one row per distinct `(sender, target)` pair.
    /// Repeating a revocation — a cut correction, a re-sent op with a new
    /// stamp, or a revoked device naming the same target over and over — no
    /// longer grows it. What it does not bound is the number of distinct
    /// targets an authenticated sender names: `apply_device_revoke` stores a
    /// row for an id no device on the account has, deliberately, because a
    /// cert can arrive after the revocation of its device. That is
    /// [`Self::cap_device_revoke_targets`]' bound, and unlike this one it
    /// does change a fold, which is why it runs before the fold and not here.
    ///
    /// Global rather than scoped to the pair the caller just wrote, so a vault
    /// that accumulated lesser rows before this existed is compacted by its
    /// next `device_revoke`, and one pass costs the same order as the fold's
    /// own sorted read.
    fn compact_device_revoke_ops(tx: &Transaction<'_>) -> rusqlite::Result<usize> {
        tx.execute(
            "DELETE FROM device_revoke_ops WHERE rowid IN (
                 SELECT rowid FROM (
                     SELECT rowid, ROW_NUMBER() OVER (
                         PARTITION BY sender, revoked_device_id
                         ORDER BY op_hlc_ms DESC, op_hlc_logical DESC
                     ) AS pos
                     FROM device_revoke_ops
                 ) WHERE pos > 1
             )",
            [],
        )
    }

    /// Keep at most [`REVOKE_TARGETS_PER_SENDER`] distinct targets per sender,
    /// and say whether the pair `(sender, target)` just written was dropped.
    ///
    /// # Why the ledger needs a cap at all
    ///
    /// [`Self::compact_device_revoke_ops`] bounds the rows per pair, not the
    /// pairs. `apply_device_revoke` has to store a row for an id no device on
    /// this account has, because a cert can arrive after the revocation of its
    /// device, so an authenticated sender naming ids it made up grew the
    /// ledger, the register, and `device_read_bounds` by one row per id, and
    /// every later fold by the same. A revoked device could do it too: its ops
    /// are stored whatever its standing. The relay cannot bound it: envelopes
    /// are opaque to it, and with `require_device_sig = false` it does not
    /// know the sender device either
    /// ([#315](https://github.com/justin13888/Sunrise/issues/315)).
    ///
    /// # What it keeps, and why that is still a function of the op set
    ///
    /// Per sender, the pairs whose greatest row sorts highest in the fold's
    /// canonical order, `(op_hlc_ms, op_hlc_logical, revoked_device_id)`
    /// descending, which is total within one sender because a pair is one
    /// sender and one target. Whole pairs are kept or dropped, so this commutes
    /// with the pair compaction. And it merges: a pair dropped here ranked
    /// below the sender's K-th pair, that K-th key only rises as rows arrive,
    /// and a later row of the dropped pair is ranked on its own stamp, which
    /// is either its pair's true greatest or lower than the stamp that already
    /// lost. So whatever order the ops arrive in, the ledger ends as the same
    /// rows, and the fold over it, which runs after this, is the same fold.
    ///
    /// # Whom it costs: only the sender over the cap
    ///
    /// It drops a sender's **own** claims and nobody else's. None of those
    /// claims can ungate that sender, because the gate on a sender is built
    /// from rows revoking it, and the discount of one of its revokers needs a
    /// row whose sender is not that sender; so a device cannot use the cap to
    /// escape [`Self::refold_device_revocations`]' gate. What it can do is
    /// withdraw its own earlier revocations by naming
    /// [`REVOKE_TARGETS_PER_SENDER`] new ids. Those revocations are already
    /// hostage to that device's standing, since the moment it is revoked they
    /// unwind anyway, and a device the account still trusts holds keys to
    /// everything. The remedy is this family's usual one: revoke again from a
    /// device the account still trusts. [`Self::revoke_device`] refuses a
    /// target with no row in `devices`, so an honest device reaches the cap
    /// only by revoking that many devices that really paired.
    fn cap_device_revoke_targets(
        tx: &Transaction<'_>,
        sender: &[u8; 16],
        target: &[u8; 16],
    ) -> rusqlite::Result<bool> {
        tx.execute(
            "WITH heads AS (
                 SELECT sender, revoked_device_id, op_hlc_ms, op_hlc_logical
                 FROM (
                     SELECT sender, revoked_device_id, op_hlc_ms, op_hlc_logical,
                            ROW_NUMBER() OVER (
                                PARTITION BY sender, revoked_device_id
                                ORDER BY op_hlc_ms DESC, op_hlc_logical DESC
                            ) AS pos
                     FROM device_revoke_ops
                 ) WHERE pos = 1
             ),
             ranked AS (
                 SELECT sender, revoked_device_id,
                        ROW_NUMBER() OVER (
                            PARTITION BY sender
                            ORDER BY op_hlc_ms DESC, op_hlc_logical DESC,
                                     revoked_device_id DESC
                        ) AS pos
                 FROM heads
             )
             DELETE FROM device_revoke_ops
             WHERE EXISTS (
                 SELECT 1 FROM ranked r
                 WHERE r.pos > ?1
                   AND r.sender = device_revoke_ops.sender
                   AND r.revoked_device_id = device_revoke_ops.revoked_device_id
             )",
            params![REVOKE_TARGETS_PER_SENDER],
        )?;
        let kept: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM device_revoke_ops
                 WHERE sender = ?1 AND revoked_device_id = ?2 LIMIT 1",
                params![&sender[..], &target[..]],
                |r| r.get(0),
            )
            .optional()?;
        Ok(kept.is_none())
    }

    /// Delete each `device_read_bounds` row whose device this replica holds no
    /// cert for and no ledger row names.
    ///
    /// The fold bounds every device its register holds, so without this the
    /// cap in [`Self::cap_device_revoke_targets`] bounded the ledger and the
    /// register and left the read bound growing by one row per made-up id: a
    /// sender over the cap names a new one, the fold lands it, and the next
    /// op evicts it from the ledger while its bound stays.
    ///
    /// **Why this does not release a bound that bounds anything.** The four
    /// key-distribution sites seal only to devices in `devices`, and nothing
    /// deletes from `devices`, so a device this replica holds a cert for is
    /// never touched here: an unwind still cannot readmit it. A row goes only
    /// when its id is certless *and* named by no ledger row, and since the
    /// pair compaction keeps every pair, the only thing that takes the last row
    /// naming an id is the per-sender cap. So what this releases is a bound on
    /// an id that only a sender over the cap ever revoked, and that no device
    /// here has. If its cert arrives later, the id is current, which is what
    /// the capped ledger says of it, and a replica that held the cert first
    /// keeps its bound. That is the non-convergence the bound already has
    /// ([#282](https://github.com/justin13888/Sunrise/issues/282)), reached
    /// here only through a sender that named more ids than the cap.
    fn release_orphan_read_bounds(tx: &Transaction<'_>) -> rusqlite::Result<usize> {
        tx.execute(
            "DELETE FROM device_read_bounds
             WHERE NOT EXISTS (
                       SELECT 1 FROM devices d
                       WHERE d.device_id = device_read_bounds.device_id
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM device_revoke_ops o
                       WHERE o.revoked_device_id = device_read_bounds.device_id
                   )",
            [],
        )
    }
}

/// The most distinct targets one sender's `device_revoke` ops keep in the
/// ledger. See [`Engine::cap_device_revoke_targets`].
///
/// Far above what an honest device reaches: [`Engine::revoke_device`] refuses
/// a target this replica holds no cert for, so an honest device gets here only
/// by revoking this many devices that really paired with the account. The
/// ledger is then at most this times the number of senders, and so is each
/// fold's work.
pub(super) const REVOKE_TARGETS_PER_SENDER: i64 = 256;
