//! What the owner's devices log inside a shared Stream for its recipients
//! (ADR-0076): the writer certs that let a recipient verify the owner's ops,
//! and a copy of each re-sealed grant under the epoch its recipient holds.
//!
//! A recipient reads only the shared Stream's own channel (ADR-0074). The
//! account's device certs and re-sealed grants are on the vault-meta stream,
//! sealed under keys it never holds, so both are copied into the Stream:
//!
//! - **A writer cert per `(Stream, epoch, device)`.** Before a device logs
//!   anything into a Stream with a share in force, under epoch `e`, it logs
//!   its own cert there under `e`, unless its cert is already in the Stream's
//!   log under `e` ([`Engine::ensure_own_writer_cert`], from `log_slot` and
//!   the digest). Per epoch, because a recipient holds only the epochs it
//!   was sealed. Sharing logs, under the live epoch, the cert of every device
//!   with an op in the Stream that this replica holds a cert for
//!   ([`Engine::log_writer_certs_for_share`]), and a device that learns of a
//!   share it has written into logs its own.
//! - **A grant copy per re-seal.** Each re-seal `ShareGrant` is logged again
//!   in the Stream as a `StreamShareGrant`, sealed under every key of the
//!   share's previous epoch, after the emitter's own cert under that epoch
//!   ([`Engine::log_stream_share_grant`]).
//!
//! Each carries the account's identity chain record, so a recipient that
//! knows only the granter's `ID_S_pub` reaches every identity that certified
//! a writer or signed a later grant. The account's own replicas apply both
//! and store nothing: the vault-meta log already tells them the same.

use super::merge::to_sql;
use super::share_grant::unshareable;
use super::{Engine, META_STREAM};
use crate::bytes::{hex_short, to16};
use crate::control_op::{StreamShareGrantPayload, StreamWriterCertPayload};
use crate::inner_op::{encode_inner_op, InnerOp};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sunrise_crypto::{decode_envelope, StreamKey};

/// The op-log `inner_kind` of a writer cert.
const WRITER_CERT_KIND: &str = "stream.writer_cert";

/// Why a copy for a shared Stream's recipients was not logged.
fn skipped(reason: &str, stream_id: &[u8; 16], epoch: u32) {
    tracing::warn!(
        ev = "core.share.stream_copy_skipped",
        reason,
        stream_h = hex_short(stream_id),
        epoch,
        "a writer cert or grant copy for a shared Stream's recipients was not logged"
    );
}

impl Engine {
    /// Whether `stream_id` has a share in force on this replica. Cheap where
    /// the account has never shared anything.
    pub(super) fn has_share_in_force(
        &self,
        conn: &Connection,
        stream_id: &[u8; 16],
    ) -> rusqlite::Result<bool> {
        if unshareable(stream_id) {
            return Ok(false);
        }
        let any: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ops WHERE stream_id = ? AND inner_kind = 'share.grant')",
            params![&META_STREAM[..]],
            |r| r.get(0),
        )?;
        if !any {
            return Ok(false);
        }
        Ok(!self
            .stream_shares_on(conn, stream_id)
            .map_err(to_sql)?
            .is_empty())
    }

    /// Log this device's own cert into `stream_id` under `(epoch, key)`
    /// where the Stream has a share in force and the cert is not there under
    /// `epoch` yet. What every write into a Stream does first.
    pub(super) fn ensure_own_writer_cert(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        (epoch, key): (u32, &StreamKey),
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        if unshareable(stream_id) {
            return Ok(());
        }
        let me = self.keychain.device_id();
        if writer_cert_logged(tx, stream_id, &me, epoch)?
            || !self.has_share_in_force(tx, stream_id)?
        {
            return Ok(());
        }
        let cert = self.keychain.cert_blob();
        self.log_writer_cert(tx, stream_id, cert, (epoch, key), now_ms)
    }

    /// Log, under the live `(epoch, key)` of `stream_id`, the cert of every
    /// device with an op in the Stream whose cert this replica holds, and
    /// this device's own, each unless already there under `epoch`. What
    /// sharing a Stream does, so a sibling that wrote under the live epoch
    /// and will not write again is still verifiable.
    pub(super) fn log_writer_certs_for_share(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        (epoch, key): (u32, &StreamKey),
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        let me = self.keychain.device_id();
        let mut devices: Vec<[u8; 16]> = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT device_id FROM ops WHERE stream_id = ? ORDER BY device_id",
            )?;
            let rows = stmt.query_map(params![&stream_id[..]], |r| r.get::<_, Vec<u8>>(0))?;
            rows.filter_map(|r| r.map(|b| to16(&b)).transpose())
                .collect::<rusqlite::Result<_>>()?
        };
        if !devices.contains(&me) {
            devices.push(me);
        }
        for device in devices {
            if writer_cert_logged(tx, stream_id, &device, epoch)? {
                continue;
            }
            let cert = if device == me {
                Some(self.keychain.cert_blob())
            } else {
                stored_cert(tx, &device)?
            };
            // A device whose cert this replica has not received is logged
            // by the device itself, at its next write into the Stream.
            let Some(cert) = cert else {
                continue;
            };
            self.log_writer_cert(tx, stream_id, cert, (epoch, key), now_ms)?;
        }
        Ok(())
    }

    /// A device that applied a share of `stream_id` logs its own cert under
    /// the live epoch if it has written into the Stream: its earlier ops
    /// under that epoch may have reached no device that shared it.
    pub(super) fn log_writer_cert_on_learning_share(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        if unshareable(stream_id) {
            return Ok(());
        }
        let me = self.keychain.device_id();
        let wrote: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM ops WHERE stream_id = ? AND device_id = ?)",
            params![&stream_id[..], &me[..]],
            |r| r.get(0),
        )?;
        if !wrote {
            return Ok(());
        }
        let Some((epoch, key)) = self.keychain.current_stream_key_tx(tx, stream_id)? else {
            return Ok(());
        };
        self.ensure_own_writer_cert(tx, stream_id, (epoch, &key), now_ms)
    }

    /// Log `grant`, a re-seal of `stream_id` to a share whose previous grant
    /// sealed `previous_epoch`, into the Stream under every key of that
    /// epoch, each after this device's own cert under it.
    pub(super) fn log_stream_share_grant(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        previous_epoch: u32,
        grant: &[u8],
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        let keys = self.keychain.stream_keys_at(stream_id, previous_epoch);
        if keys.is_empty() {
            skipped("no_previous_key", stream_id, previous_epoch);
            return Ok(());
        }
        let Some(chain) = self.chain_bytes(tx, stream_id, previous_epoch)? else {
            return Ok(());
        };
        let me = self.keychain.device_id();
        let several = keys.len() > 1;
        for key in &keys {
            // With two keys at the epoch, the cert logged under the other
            // key is unreadable to a recipient holding this one.
            if several || !writer_cert_logged(tx, stream_id, &me, previous_epoch)? {
                let cert = self.keychain.cert_blob();
                self.log_writer_cert(tx, stream_id, cert, (previous_epoch, key), now_ms)?;
            }
            let op = InnerOp::StreamShareGrant(StreamShareGrantPayload {
                grant: grant.to_vec(),
                chain: chain.clone(),
                unknown: sunrise_domain::Unknowns::new(),
            });
            self.emit_stream_control_op(tx, stream_id, &op, (previous_epoch, key), now_ms)?;
        }
        Ok(())
    }

    /// Log `cert` into `stream_id` under `(epoch, key)`, with the chain.
    fn log_writer_cert(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        cert: Vec<u8>,
        (epoch, key): (u32, &StreamKey),
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        let Some(chain) = self.chain_bytes(tx, stream_id, epoch)? else {
            return Ok(());
        };
        let op = InnerOp::StreamWriterCert(StreamWriterCertPayload {
            cert,
            chain,
            unknown: sunrise_domain::Unknowns::new(),
        });
        self.emit_stream_control_op(tx, stream_id, &op, (epoch, key), now_ms)
    }

    /// The account's identity chain record, encoded; `None`, logged, where
    /// it does not convert or encode.
    fn chain_bytes(
        &self,
        conn: &Connection,
        stream_id: &[u8; 16],
        epoch: u32,
    ) -> rusqlite::Result<Option<Vec<u8>>> {
        let record = match self.identity_chain_record(conn) {
            Ok(Some(record)) => record,
            Ok(None) => {
                skipped("no_chain", stream_id, epoch);
                return Ok(None);
            }
            Err(e) => return Err(to_sql(e)),
        };
        let encoded = record.encode().ok();
        if encoded.is_none() {
            skipped("no_chain", stream_id, epoch);
        }
        Ok(encoded)
    }

    /// Seal `inner` into `stream_id`'s log under `(epoch, key)`, as a digest
    /// is: a control op in an entity stream, no target, this device's next
    /// `seq`.
    fn emit_stream_control_op(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        inner: &InnerOp,
        (epoch, key): (u32, &StreamKey),
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        let blob = encode_inner_op(inner).map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
        let target = inner.target_ref();
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
            Some(target.bytes()),
            Some(now_ms),
            None,
            now_ms,
            &[],
            epoch,
            key,
        )
    }
}

/// Whether `device`'s cert is in `stream_id`'s log under `epoch`.
fn writer_cert_logged(
    conn: &Connection,
    stream_id: &[u8; 16],
    device: &[u8; 16],
    epoch: u32,
) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(
        "SELECT envelope FROM ops WHERE stream_id = ? AND inner_kind = ? AND target_id = ?",
    )?;
    let rows = stmt.query_map(
        params![&stream_id[..], WRITER_CERT_KIND, &device[..]],
        |r| r.get::<_, Vec<u8>>(0),
    )?;
    for row in rows {
        let bytes: Vec<u8> = row?;
        if decode_envelope(&bytes).is_ok_and(|env| env.epoch == epoch) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The cert this replica holds for `device`, if any.
fn stored_cert(conn: &Connection, device: &[u8; 16]) -> rusqlite::Result<Option<Vec<u8>>> {
    conn.query_row(
        "SELECT cert_blob FROM devices WHERE device_id = ?",
        params![&device[..]],
        |r| r.get(0),
    )
    .optional()
}
