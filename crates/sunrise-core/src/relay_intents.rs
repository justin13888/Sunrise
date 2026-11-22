//! The `relay_revocation_intents` queue: "tell the relay this device is gone".
//!
//! Revoking a device is two facts in two places. In the vault it is a
//! `device_revoke` op sealed under the vault-meta Stream key, which the relay
//! holds no key for and so cannot read. At the relay it is a flag on its own
//! `devices` row, set through `DELETE /api/v1/devices/by-vault-id/{id}`.
//!
//! The queue is what connects them, and it is a table rather than a direct call
//! because `revoke_device` must work offline — a device that is gone is the
//! whole scenario. `crates/sunrise-storage/migrations/0020_relay_revocation_intents.sql`
//! carries the rest of the reasoning; the drain is [`drain`], which the sync
//! driver runs over the transport it holds.
//!
//! These four methods moved off `Core` rather than being written here: they are
//! the symbols that share one table, which is the cohesion boundary the
//! file-size gate names, and `core.rs`, now `core/mod.rs`, was at its threshold.
//!
//! # A row is owed only while the register agrees
//!
//! A row here is not, on its own, a revocation the relay is owed. The register
//! (`device_revocations`) is derived: `Engine::refold_device_revocations`
//! rebuilds it from the whole ledger on every applied `device_revoke`, and a
//! revocation it believed can **unwind** when its author turns out to have been
//! revoked first (ADR-0041). The fold does not touch this table, and should not
//! — so an intent can outlive the register row it was queued beside.
//!
//! [`OWED_RELAY_REVOCATIONS_SQL`] is therefore the one definition of what is
//! owed: an intent whose device the register **currently** calls revoked. The
//! drain sends only those, asking again for each device right before its send,
//! and [`Core::relay_revocation_pending`] answers from the same set, so neither
//! tells the relay to cut a device the register already calls current when the
//! send starts (#257). The row itself stays: when a later fold revokes the
//! device again — the discount can restore an unwound revocation — the intent
//! is owed again, which deleting it on the unwind would have lost, leaving the
//! relay behind the register (#160's failure).
//!
//! What no row here can do is take back a revocation the relay has already
//! been told. The drain deletes the row on the relay's answer. Only a
//! withdrawal sends the relay's inverse of `DELETE`, through the second
//! queue below; an unwind does not, so an unwind after that point leaves the
//! relay refusing the device. That matches the read bound, which an unwind
//! never releases either: an unwound device is not back in at either bound.
//! `docs/03-crypto/key-rotation.md` §Revocation states it for a reader.
//!
//! # The reinstatement queue (ADR-0056 §5)
//!
//! `relay_reinstatement_intents` is the inverse: "tell the relay this device
//! is back". `Command::WithdrawRevocation` cancels an unsent row of the first
//! queue; where there was none to cancel the relay may already have been
//! told, so the command queues a row here instead, in its own transaction
//! ([`queue_relay_reinstatement`]). [`drain_reinstatements`] sends it through
//! `POST /api/v1/devices/by-vault-id/{id}/reinstate`, which the relay accepts
//! only from the device it recorded as the revoker.
//!
//! The owed rule mirrors the first queue's: a row is owed only while the
//! register does **not** call its device revoked
//! ([`OWED_RELAY_REINSTATEMENTS_SQL`]), asked again before each send. So a
//! device another device still revokes, or this one revoked again, is not
//! reinstated at the relay, and the row is sent once the register lets it go.
//! The two queues are therefore never owed for one device at once. A new
//! `RevokeDevice` of the device deletes its row outright, so an unwind of
//! that revocation cannot send a reversal queued before it.
//!
//! The relay's `403 DEVICE_NOT_REVOKER` is terminal: another device revoked
//! it at the relay, or this device lost the key it revoked under. The row
//! stays with `refused_at_ms` set, never sent again, so
//! [`Core::relay_reinstatement_pending`] can still say the relay refuses the
//! device and that pairing it again is the remedy.

use rusqlite::{params, Connection, OptionalExtension};
use sunrise_sync::{ReinstateOutcome, RevokeOutcome, Transport, TransportError};

use crate::bytes::hex_short;
use crate::core::{Core, CoreError};

/// Tell the relay about every device this vault has revoked and not yet
/// reported.
///
/// The relay cannot learn this from the op stream — `device_revoke` is sealed
/// under the vault-meta key and the relay holds no Stream keys — so the two
/// halves of a revocation travel separately, and this is the second one. Until
/// it lands the relay keeps accepting the revoked device's uploads and keeps
/// streaming it everyone else's, which is `#80`.
///
/// It sends only what the register still agrees with. A re-fold can unwind the
/// register row an intent was queued beside, and an intent sent after that
/// would have the relay refuse a device every replica shows as current (#257).
/// Such a row is skipped, not deleted, so it is sent after all if a later fold
/// revokes the device again; [`OWED_RELAY_REVOCATIONS_SQL`] holds the one
/// definition.
/// The check is repeated for each device immediately before its send, not
/// read once for the whole drain: the sends await the network, and a local
/// command can unwind a device while an earlier one is in flight. An unwind
/// that lands during a device's own send is the case the module doc names:
/// the relay has been told, and nothing takes that back.
///
/// Failures leave the row in place, deliberately. The device this is about is
/// typically lost or stolen; giving up after one refused call would mean the
/// revocation silently never reached the relay, which is the failure this whole
/// mechanism exists to prevent. `attempts` is what makes a relay that keeps
/// refusing visible in the log rather than retried in silence.
///
/// [`RevokeOutcome::Unknown`] clears the row and warns rather than reporting
/// success. It is terminal — the relay holds no active row with that vault id
/// and retrying cannot make one appear — but it is *not* the outcome the user
/// asked for: it is also the answer for a device that registered before
/// `vault_device_id` existed, which the relay is still accepting under a row
/// this vault cannot name. Reading it as success is the exact defect PR #86
/// withdrew.
///
/// A [`TransportError::Unsupported`] transport is not an attempt: the loopback
/// and in-process transports have no account API, and counting them would
/// inflate `attempts` on every convergence test that revokes anything.
pub(crate) async fn drain<T: Transport + ?Sized>(core: &Core, transport: &mut T) {
    let Ok(pending) = core.pending_relay_revocations() else {
        return;
    };
    for device_id in pending {
        // The snapshot above is taken once, and every send below awaits the
        // network, so a local command can unwind a later device's register row
        // while an earlier one is in flight. Ask again right before each send.
        match core.relay_revocation_pending(&device_id) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => return,
        }
        match transport.revoke_device(device_id).await {
            Ok(RevokeOutcome::Revoked) => {
                // No row left to clear means a withdrawal cancelled it while
                // the send was in flight, believing the relay had not been
                // told. It has now, so the reinstatement that withdrawal did
                // not queue is queued here.
                if let Ok(false) = core.clear_relay_revocation(device_id) {
                    let _ = core.queue_relay_reinstatement_after_cancel(device_id);
                }
                tracing::info!(
                    ev = "sync.device.revoke_relayed",
                    subject_h = hex_short(&device_id),
                    "the relay has been told to stop accepting a revoked device"
                );
            }
            Ok(RevokeOutcome::Unknown) => {
                let _ = core.clear_relay_revocation(device_id);
                tracing::warn!(
                    ev = "sync.device.revoke_unknown_to_relay",
                    subject_h = hex_short(&device_id),
                    "the relay holds no device bound to this vault device id; if that device \
                     registered before it sent one, the relay is still accepting it"
                );
            }
            Err(TransportError::Unsupported) => return,
            Err(e) => {
                let attempts = core
                    .note_relay_revocation_attempt(device_id, core.now_ms())
                    .unwrap_or(0);
                tracing::warn!(
                    ev = "sync.device.revoke_not_relayed",
                    subject_h = hex_short(&device_id),
                    attempt = attempts,
                    cause = %e,
                    "the relay has not accepted a device revocation; it still accepts that device"
                );
            }
        }
    }
}

/// The intents the relay is owed, oldest first: rows of
/// `relay_revocation_intents` whose device the register currently calls
/// revoked. See the module doc for why presence alone is not enough.
pub(crate) const OWED_RELAY_REVOCATIONS_SQL: &str = "SELECT i.device_id
     FROM relay_revocation_intents i
     WHERE EXISTS (SELECT 1 FROM device_revocations r WHERE r.device_id = i.device_id)
     ORDER BY i.created_at_ms ASC, i.device_id ASC";

/// Ask the relay to accept again every device whose revocation this device
/// withdrew and the relay may already have been told (ADR-0056 §5).
///
/// The counterpart of [`drain`], run beside it, after it, by the sync driver.
/// It sends only what [`OWED_RELAY_REINSTATEMENTS_SQL`] calls owed, asking
/// again for each device right before its send, for the reason [`drain`]
/// gives: a local command can revoke the device again while an earlier send
/// is in flight.
///
/// - [`ReinstateOutcome::Reinstated`] clears the row.
/// - [`ReinstateOutcome::NotRevoker`] is terminal and not success: the relay
///   recorded another revoker, or none, and keeps refusing the device. The
///   row stays, marked refused, so the seam can say so; it is never sent
///   again.
/// - [`ReinstateOutcome::Unknown`] is terminal and not success: the relay
///   holds no row with that vault id, which says nothing about whether it
///   accepts the device. The row is cleared and the drain warns.
/// - [`TransportError::Unsupported`] is not an attempt, as in [`drain`].
/// - Any other error counts an attempt and keeps the row, including the
///   relay's `409 DEVICE_REINSTATE_PREDATES_REVOKE`, which a request signed
///   again later clears.
pub(crate) async fn drain_reinstatements<T: Transport + ?Sized>(core: &Core, transport: &mut T) {
    let Ok(pending) = core.owed_relay_reinstatements() else {
        return;
    };
    for device_id in pending {
        match core.owed_relay_reinstatements() {
            Ok(owed) if owed.contains(&device_id) => {}
            Ok(_) => continue,
            Err(_) => return,
        }
        match transport.reinstate_device(device_id).await {
            Ok(ReinstateOutcome::Reinstated) => {
                let _ = core.clear_relay_reinstatement(device_id);
                tracing::info!(
                    ev = "sync.device.reinstate_relayed",
                    subject_h = hex_short(&device_id),
                    "the relay accepts a device whose revocation this device withdrew"
                );
            }
            Ok(ReinstateOutcome::NotRevoker) => {
                let _ = core.refuse_relay_reinstatement(device_id, core.now_ms());
                tracing::warn!(
                    ev = "sync.device.reinstate_refused_by_relay",
                    subject_h = hex_short(&device_id),
                    "the relay recorded another revoker for this device and keeps refusing it; \
                     pair it again"
                );
            }
            Ok(ReinstateOutcome::Unknown) => {
                let _ = core.clear_relay_reinstatement(device_id);
                tracing::warn!(
                    ev = "sync.device.reinstate_unknown_to_relay",
                    subject_h = hex_short(&device_id),
                    "the relay holds no device bound to this vault device id, so there was \
                     nothing to reinstate"
                );
            }
            Err(TransportError::Unsupported) => return,
            Err(e) => {
                let attempts = core
                    .note_relay_reinstatement_attempt(device_id, core.now_ms())
                    .unwrap_or(0);
                tracing::warn!(
                    ev = "sync.device.reinstate_not_relayed",
                    subject_h = hex_short(&device_id),
                    attempt = attempts,
                    cause = %e,
                    "the relay has not accepted a reinstatement; it still refuses that device"
                );
            }
        }
    }
}

/// The reinstatements the relay is owed, oldest first: rows of
/// `relay_reinstatement_intents` it has not refused, whose device the
/// register does **not** call revoked. See the module doc.
pub(crate) const OWED_RELAY_REINSTATEMENTS_SQL: &str = "SELECT i.device_id
     FROM relay_reinstatement_intents i
     WHERE i.refused_at_ms IS NULL
       AND NOT EXISTS (SELECT 1 FROM device_revocations r WHERE r.device_id = i.device_id)
     ORDER BY i.created_at_ms ASC, i.device_id ASC";

/// Queue a reinstatement of `device_id`, inside the caller's transaction.
///
/// One row per device. A row already there, sent or refused, is replaced by a
/// fresh one: this is a new withdrawal, of a revocation the relay may have
/// recorded under this device's current key, so an earlier refusal says
/// nothing about it.
pub(crate) fn queue_relay_reinstatement(
    conn: &Connection,
    device_id: &[u8; 16],
    now_ms: u64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO relay_reinstatement_intents (device_id, created_at_ms) VALUES (?1, ?2)
         ON CONFLICT (device_id) DO UPDATE SET
             created_at_ms = excluded.created_at_ms,
             attempts = 0,
             last_attempt_ms = NULL,
             refused_at_ms = NULL",
        params![&device_id[..], i64::try_from(now_ms).unwrap_or(i64::MAX)],
    )?;
    Ok(())
}

/// Where the relay's half of a withdrawal stands, for one device
/// (ADR-0056 §5): what [`Core::relay_reinstatement_pending`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayReinstatement {
    /// Nothing is owed to the relay for this device: no withdrawal queued a
    /// reinstatement, the relay accepted it, the relay held no row with that
    /// id, or the register calls the device revoked again.
    Clear,
    /// A reinstatement is queued and will be sent on the next session. Until
    /// then a relay that was told of the revocation still refuses the device.
    Pending,
    /// The relay refused the reinstatement: it recorded another device as
    /// the revoker, or none, or this device no longer holds the key it
    /// revoked under. Terminal; the relay keeps refusing the device, and the
    /// remedy is pairing it again.
    Refused,
}

impl Core {
    /// Devices this vault has revoked and not yet told the relay about.
    ///
    /// The 16-byte ids are the vault's own, which is the only name a revoking
    /// device holds for a peer and the one
    /// `DELETE /api/v1/devices/by-vault-id/{id}` takes.
    ///
    /// Only the intents the register still agrees with: an intent whose
    /// register row a re-fold unwound stays queued and is not returned until
    /// the register revokes that device again. See the module doc.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn pending_relay_revocations(&self) -> Result<Vec<[u8; 16]>, CoreError> {
        let db = self.db();
        let mut stmt = db.conn().prepare(OWED_RELAY_REVOCATIONS_SQL)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|id| <[u8; 16]>::try_from(id.as_slice()).ok())
            .collect())
    }

    /// Whether a revocation of `device_id` is still owed to the relay.
    ///
    /// The two halves of a revocation are different guarantees and a user is
    /// entitled to know which they have: the vault half is committed the
    /// moment `RevokeDevice` returns, while the relay half is a queued intent
    /// that needs a session. A client that reported only the first would let
    /// someone with no network believe a stolen laptop had been cut off from
    /// the server, which it has not been. See issue #160.
    ///
    /// Answered from the same set the drain sends, so "queued" here always
    /// means "will be sent": an intent the register no longer agrees with
    /// (#257) is not pending. That is also why a `RevokeDevice` the fold
    /// discarded can never read as pending — its target is current, whatever
    /// an earlier revocation left in the table.
    ///
    /// # Errors
    /// Storage failures.
    pub fn relay_revocation_pending(&self, device_id: &[u8; 16]) -> Result<bool, CoreError> {
        self.ensure_open()?;
        Ok(self.pending_relay_revocations()?.contains(device_id))
    }

    /// Forget an intent the relay has answered. Returns whether there was a
    /// row to forget: `false` means a withdrawal cancelled it first.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn clear_relay_revocation(&self, device_id: [u8; 16]) -> Result<bool, CoreError> {
        let db = self.db();
        let n = db.conn().execute(
            "DELETE FROM relay_revocation_intents WHERE device_id = ?",
            rusqlite::params![&device_id[..]],
        )?;
        Ok(n > 0)
    }

    /// Queue the reinstatement a withdrawal did not, because it cancelled a
    /// revocation the drain was sending at that moment.
    ///
    /// # Errors
    /// Storage failures.
    fn queue_relay_reinstatement_after_cancel(&self, device_id: [u8; 16]) -> Result<(), CoreError> {
        let now_ms = self.now_ms();
        let db = self.db();
        queue_relay_reinstatement(db.conn(), &device_id, now_ms)?;
        Ok(())
    }

    /// Record that an attempt was made and refused, so a relay that keeps
    /// saying no is visible rather than retried in silence.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn note_relay_revocation_attempt(
        &self,
        device_id: [u8; 16],
        now_ms: u64,
    ) -> Result<u64, CoreError> {
        let db = self.db();
        db.conn().execute(
            "UPDATE relay_revocation_intents
             SET attempts = attempts + 1, last_attempt_ms = ?
             WHERE device_id = ?",
            rusqlite::params![i64::try_from(now_ms).unwrap_or(i64::MAX), &device_id[..]],
        )?;
        let attempts: i64 = db.conn().query_row(
            "SELECT attempts FROM relay_revocation_intents WHERE device_id = ?",
            rusqlite::params![&device_id[..]],
            |r| r.get(0),
        )?;
        Ok(u64::try_from(attempts).unwrap_or(0))
    }

    /// Devices the relay is owed a reinstatement of, oldest first. See
    /// [`OWED_RELAY_REINSTATEMENTS_SQL`].
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn owed_relay_reinstatements(&self) -> Result<Vec<[u8; 16]>, CoreError> {
        let db = self.db();
        let mut stmt = db.conn().prepare(OWED_RELAY_REINSTATEMENTS_SQL)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|id| <[u8; 16]>::try_from(id.as_slice()).ok())
            .collect())
    }

    /// Where the relay's half of withdrawing `device_id`'s revocation stands.
    ///
    /// The counterpart of [`Self::relay_revocation_pending`] (#160), for the
    /// same reason: `WithdrawRevocation` commits the vault half when it
    /// returns, while a relay that was already told keeps refusing the device
    /// until a session sends the reinstatement. A client that reported only
    /// the first would tell the user a device is back that cannot sync.
    ///
    /// [`RelayReinstatement::Pending`] is answered from the same set the
    /// drain sends, so it always means "will be sent".
    /// [`RelayReinstatement::Refused`] is the relay's terminal refusal, kept
    /// after the drain ran so the remedy, pairing the device again, can still
    /// be offered. Both read as [`RelayReinstatement::Clear`] while the
    /// register calls the device revoked: the device is not back at all, and
    /// the device list already says so.
    ///
    /// # Errors
    /// Storage failures.
    pub fn relay_reinstatement_pending(
        &self,
        device_id: &[u8; 16],
    ) -> Result<RelayReinstatement, CoreError> {
        self.ensure_open()?;
        let db = self.db();
        let row: Option<Option<i64>> = db
            .conn()
            .query_row(
                "SELECT i.refused_at_ms
                 FROM relay_reinstatement_intents i
                 WHERE i.device_id = ?
                   AND NOT EXISTS
                       (SELECT 1 FROM device_revocations r WHERE r.device_id = i.device_id)",
                params![&device_id[..]],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match row {
            None => RelayReinstatement::Clear,
            Some(None) => RelayReinstatement::Pending,
            Some(Some(_)) => RelayReinstatement::Refused,
        })
    }

    /// Forget a reinstatement the relay has accepted, or has no row for.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn clear_relay_reinstatement(&self, device_id: [u8; 16]) -> Result<(), CoreError> {
        let db = self.db();
        db.conn().execute(
            "DELETE FROM relay_reinstatement_intents WHERE device_id = ?",
            params![&device_id[..]],
        )?;
        Ok(())
    }

    /// Record the relay's terminal refusal of a reinstatement, keeping the row
    /// as its trace and taking it out of what the drain sends.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn refuse_relay_reinstatement(
        &self,
        device_id: [u8; 16],
        now_ms: u64,
    ) -> Result<(), CoreError> {
        let now = i64::try_from(now_ms).unwrap_or(i64::MAX);
        let db = self.db();
        db.conn().execute(
            "UPDATE relay_reinstatement_intents
             SET attempts = attempts + 1, last_attempt_ms = ?1, refused_at_ms = ?1
             WHERE device_id = ?2",
            params![now, &device_id[..]],
        )?;
        Ok(())
    }

    /// Record a reinstatement the relay did not accept this time, so a relay
    /// that keeps saying no is visible rather than retried in silence.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn note_relay_reinstatement_attempt(
        &self,
        device_id: [u8; 16],
        now_ms: u64,
    ) -> Result<u64, CoreError> {
        let db = self.db();
        db.conn().execute(
            "UPDATE relay_reinstatement_intents
             SET attempts = attempts + 1, last_attempt_ms = ?
             WHERE device_id = ?",
            params![i64::try_from(now_ms).unwrap_or(i64::MAX), &device_id[..]],
        )?;
        let attempts: i64 = db.conn().query_row(
            "SELECT attempts FROM relay_reinstatement_intents WHERE device_id = ?",
            params![&device_id[..]],
            |r| r.get(0),
        )?;
        Ok(u64::try_from(attempts).unwrap_or(0))
    }

    /// Queue a reinstatement without going through
    /// `Command::WithdrawRevocation`, for the driver's tests, which have no
    /// second device to revoke and withdraw. Test-only.
    #[cfg(test)]
    pub(crate) fn queue_relay_reinstatement_for_test(
        &self,
        device_id: [u8; 16],
    ) -> Result<(), CoreError> {
        let db = self.db();
        queue_relay_reinstatement(db.conn(), &device_id, 1)?;
        Ok(())
    }

    /// The `attempts` of `device_id`'s reinstatement row, or `None` when
    /// there is no row. Test-only.
    #[cfg(test)]
    pub(crate) fn relay_reinstatement_attempts_for_test(
        &self,
        device_id: [u8; 16],
    ) -> Result<Option<i64>, CoreError> {
        let db = self.db();
        Ok(db
            .conn()
            .query_row(
                "SELECT attempts FROM relay_reinstatement_intents WHERE device_id = ?",
                params![&device_id[..]],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Queue a relay revocation without going through `Command::RevokeDevice`.
    ///
    /// Test-only. The command refuses a device this vault has never admitted,
    /// which is the right rule and makes the driver's own tests — which have
    /// no second device to admit — unable to reach the queue at all.
    ///
    /// Writes the register row beside the intent, because an intent is owed
    /// only while the register calls its device revoked (module doc). The row
    /// is a placeholder, not a fold's output: nothing here reads its fields.
    #[cfg(test)]
    pub(crate) fn queue_relay_revocation_for_test(
        &self,
        device_id: [u8; 16],
    ) -> Result<(), CoreError> {
        let db = self.db();
        db.conn().execute(
            "INSERT OR IGNORE INTO relay_revocation_intents (device_id, created_at_ms)
             VALUES (?, ?)",
            rusqlite::params![&device_id[..], 1_i64],
        )?;
        db.conn().execute(
            "INSERT OR IGNORE INTO device_revocations
             (device_id, cut_ms, cut_logical, revoked_by, reason, recorded_at_ms)
             VALUES (?, 1, 0, X'', 'Lost', 1)",
            rusqlite::params![&device_id[..]],
        )?;
        Ok(())
    }

    /// Drop `device_id`'s register row and nothing else: what a re-fold
    /// unwinding that revocation leaves behind. Test-only.
    #[cfg(test)]
    pub(crate) fn unwind_register_row_for_test(
        &self,
        device_id: [u8; 16],
    ) -> Result<(), CoreError> {
        let db = self.db();
        db.conn().execute(
            "DELETE FROM device_revocations WHERE device_id = ?",
            rusqlite::params![&device_id[..]],
        )?;
        Ok(())
    }

    /// Drop the register table, so every read of what is owed fails at
    /// storage. Test-only.
    #[cfg(test)]
    pub(crate) fn break_register_for_test(&self) -> Result<(), CoreError> {
        let db = self.db();
        db.conn().execute("DROP TABLE device_revocations", [])?;
        Ok(())
    }

    /// Whether an intent row exists for `device_id`, owed or not. Test-only.
    #[cfg(test)]
    pub(crate) fn relay_intent_row_exists_for_test(
        &self,
        device_id: [u8; 16],
    ) -> Result<bool, CoreError> {
        let db = self.db();
        let n: i64 = db.conn().query_row(
            "SELECT count(*) FROM relay_revocation_intents WHERE device_id = ?",
            rusqlite::params![&device_id[..]],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// How many times the relay has refused this revocation. Test-only.
    #[cfg(test)]
    pub(crate) fn relay_revocation_attempts_for_test(
        &self,
        device_id: [u8; 16],
    ) -> Result<i64, CoreError> {
        let db = self.db();
        Ok(db.conn().query_row(
            "SELECT attempts FROM relay_revocation_intents WHERE device_id = ?",
            rusqlite::params![&device_id[..]],
            |r| r.get(0),
        )?)
    }
}
