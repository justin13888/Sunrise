//! Parked ops: the remote ops this build keeps without applying them.
//!
//! An op that verified and opened but that this build cannot read, because
//! its inner kind is unknown here or its schema fingerprint disagrees with
//! this build's registry (ADR-0045 §3, §4), goes into `ops` under
//! [`PARKED_KIND`] with a `parked_ops` marker, and is retried through the
//! apply path when a build with a different `DOC_SCHEMA_V` opens the vault.
//! Everything that writes, re-reasons or replays a marker is here; deciding
//! that an op parks is the receive path's, in `sync`.

use super::chain::{check_covered, check_duplicate, check_links};
use super::ids::hex_short;
use super::oplog::{remote_op_id, upsert_sync_cursor};
use super::sync::oplog_to_sqlite;
use super::{Engine, EngineError};
use crate::events::DomainEvent;
use sunrise_cbor::version::{doc_schema_fp_prefix, DOC_SCHEMA_V};
use sunrise_crypto::decode_envelope;
use sunrise_storage::{Db, OpLog, ParkReason, Parking, PARKED_KIND};

/// Log a storage failure that stopped a parked-op replay at `step` (`read`,
/// `apply` or `restamp`). The op keeps its stamp, so the next open retries
/// it. A class and not the message: a storage error can quote a bound value.
fn log_replay_failed(
    header: Option<&sunrise_crypto::OpEnvelope>,
    step: &'static str,
    e: &EngineError,
) {
    tracing::warn!(
        ev = "core.op.parked_replay_failed",
        reason = step,
        stream_h = header.map(|h| hex_short(&h.stream_id)).unwrap_or_default(),
        sender_h = header.map(|h| hex_short(&h.device_id)).unwrap_or_default(),
        seq = header.map_or(0, |h| h.seq),
        cause = storage_class(e),
        "a parked op could not be replayed; it stays parked and the next open retries it"
    );
}

/// The error class a replay-failure log carries in place of the message.
const fn storage_class(e: &EngineError) -> &'static str {
    match e {
        EngineError::Storage(_) => "storage",
        EngineError::Sqlite(_) => "sqlite",
        EngineError::OpLog(_) => "oplog",
        _ => "other",
    }
}

/// Whether `env`'s field 13 disagrees with this build's registry
/// (ADR-0045 §3): its `doc_schema_v` has an entry here, and field 13 is not
/// that entry's prefix, including when it is missing. A version with no
/// entry, one before `DOC_SCHEMA_FP_FIRST` or one newer than this build, never
/// mismatches: there is nothing here to compare it with.
pub(super) fn schema_fp_mismatch(env: &sunrise_crypto::OpEnvelope) -> bool {
    doc_schema_fp_prefix(env.doc_schema_v).is_some_and(|want| env.schema_fp != Some(want))
}

/// The variant name an inner op's bytes are tagged with, read without
/// decoding the payload, for a parked row's diagnostic `kind`; empty when the
/// bytes are not an externally tagged op. A fingerprint-mismatched op is
/// parked before it is decoded, because its writer may mean its version
/// differently, so this is the only name it can be recorded under.
pub(super) fn diagnostic_kind(inner_cbor: &[u8]) -> String {
    use ciborium::value::Value;
    match ciborium::de::from_reader::<Value, _>(inner_cbor) {
        Ok(Value::Map(mut entries)) if entries.len() == 1 => match entries.pop() {
            Some((Value::Text(name), _)) => name,
            _ => String::new(),
        },
        Ok(Value::Text(name)) => name,
        _ => String::new(),
    }
}

impl Engine {
    /// Keep an op that verified and opened but whose inner kind this build
    /// does not know (issue #320, ADR-0045 §4).
    ///
    /// The op goes into `ops` like any delivery, unapplied and under
    /// [`PARKED_KIND`], and `parked_ops` marks it with the build that parked
    /// it. Being in `ops` is what makes it count as received:
    /// [`upsert_sync_cursor`] runs here exactly as it does on the apply path,
    /// so the relay stops re-sending the op and the seqs above it are not held
    /// back behind it. Nothing is refused and nothing is reported as damage.
    ///
    /// Unlike [`Self::defer_op`] this has no TTL and no cap. What reaches it
    /// has passed the signature check and the AEAD open, which is the standing
    /// that writes any op into `ops` for good, and it has advanced the cursor,
    /// so nothing would ever send it again: dropping it would be permanent.
    ///
    /// The sender's stamp **is** observed, unlike a deferred op's, and before
    /// the row is written: the row is in `ops`, which is what
    /// [`Self::prime_hlc`] restores the clock from, so the clock has to have
    /// seen it or the next open would put the process above a reading this
    /// session never took. A stamp beyond the drift window is refused here as
    /// it is on the apply path, and leaves no row.
    ///
    /// `doc_schema_v` is the parking build's `DOC_SCHEMA_V`, a parameter only
    /// so a test can write the row an older build would have.
    pub(crate) fn park_op(
        &self,
        db: &mut Db,
        envelope_bytes: &[u8],
        env: &sunrise_crypto::OpEnvelope,
        kind: &str,
        doc_schema_v: u16,
    ) -> Result<(), EngineError> {
        self.park_op_as(
            db,
            envelope_bytes,
            env,
            ParkReason::UnknownKind,
            kind,
            doc_schema_v,
        )
    }

    /// [`Self::park_op`] for any first-delivery reason: an unknown kind, or a
    /// schema-fingerprint mismatch (ADR-0045 §3).
    ///
    /// A re-delivery of an op that is already parked, with the same bytes,
    /// writes no new row, and replaces the row's reason with `reason`: it is
    /// why this build keeps the op now, which is what a replay that still
    /// cannot apply it must record (ADR-0045 §4).
    pub(crate) fn park_op_as(
        &self,
        db: &mut Db,
        envelope_bytes: &[u8],
        env: &sunrise_crypto::OpEnvelope,
        reason: ParkReason,
        kind: &str,
        doc_schema_v: u16,
    ) -> Result<(), EngineError> {
        self.hlc
            .observe(env.hlc)
            .map_err(|e| EngineError::RemoteOpInvalid(format!("hlc: {e}")))?;
        let op_id = remote_op_id(&env.stream_id, &env.device_id, env.seq);
        let now_ms = self.clock.now_ms();
        let mut parked = false;
        db.with_tx(|tx| {
            // A position a compaction floor covers is an op this replica
            // folded, so an op there is a duplicate, or fork evidence at the
            // floor itself, whatever its kind: parking it would put a row below
            // the floor for an upgrade to replay (ADR-0059 §2).
            if check_covered(tx, env, envelope_bytes, now_ms)? {
                return Ok(());
            }
            OpLog::insert(
                tx,
                &op_id,
                &env.stream_id,
                &env.device_id,
                env.seq,
                env.hlc.physical_ms,
                envelope_bytes,
                PARKED_KIND,
                PARKED_KIND,
                None,
                None,
                Some(&env.device_id),
                now_ms,
                &[],
            )
            .map_err(oplog_to_sqlite)?;
            // A re-delivery of an op already in the log, parked or not, adds
            // no row: the row and its marker are the first delivery's, and
            // only a parked marker's reason moves to the cause found now. A
            // different op at that position is fork evidence (ADR-0043 §4).
            if tx.changes() == 0 {
                OpLog::repark(tx, &op_id, envelope_bytes, reason).map_err(oplog_to_sqlite)?;
                check_duplicate(tx, env, envelope_bytes, now_ms)?;
                return Ok(());
            }
            // A parked op is in the log and counts toward the prefix, so its
            // links are checked now, like an applied op's (ADR-0043 §3).
            check_links(tx, &op_id, env, envelope_bytes, now_ms)?;
            OpLog::park(
                tx,
                &Parking {
                    op_id: &op_id,
                    reason,
                    kind,
                    hlc_logical: env.hlc.logical,
                    doc_schema_v,
                    parked_at_ms: now_ms,
                },
            )
            .map_err(oplog_to_sqlite)?;
            upsert_sync_cursor(tx, &env.stream_id, &env.device_id, now_ms)?;
            parked = true;
            Ok(())
        })?;
        if parked {
            if reason == ParkReason::SchemaFpMismatch {
                tracing::warn!(
                    ev = "core.op.schema_fp_mismatch",
                    stream_h = hex_short(&env.stream_id),
                    sender_h = hex_short(&env.device_id),
                    seq = env.seq,
                    doc_v = env.doc_schema_v,
                    kind = if env.schema_fp.is_some() {
                        "differs"
                    } else {
                        "missing"
                    },
                    "kept an op whose schema fingerprint disagrees with this build's registry; \
                     it replays when the registry changes"
                );
            } else {
                tracing::info!(
                    ev = "core.op.parked",
                    reason = reason.as_str(),
                    stream_h = hex_short(&env.stream_id),
                    "kept an op of a kind this build does not know; it replays after an upgrade"
                );
            }
        }
        Ok(())
    }

    /// Retry every parked op that a build with a different `DOC_SCHEMA_V`
    /// parked or last tried, through the full apply path, in
    /// `(hlc, device_id, seq)` order. Returns the events the released ops
    /// produced.
    ///
    /// [`crate::Core::open`] calls this, which is what "replayed after an
    /// upgrade" means: a vault whose parked ops were all tried by this build
    /// reads one indexed query and returns.
    ///
    /// Each op goes back through [`Self::apply_remote_all`] with the bytes it
    /// was parked with, so it meets the signature, key, clock and LWW gates
    /// exactly as a first delivery would. What releases it is that path's
    /// idempotence gate: the `ops` row is already there, so the insert changes
    /// nothing, and [`OpLog::unpark`] turns the parked row into an applied one
    /// in the transaction that materializes it. Replay is therefore idempotent
    /// the way apply is: a released op is an ordinary applied op, and a second
    /// replay, or the relay re-sending it, returns at the same gate.
    ///
    /// An op this build still cannot apply stays parked, and is re-stamped
    /// with this build so the next open does not try it again. That covers a
    /// kind this build does not know, or a fingerprint this build's registry
    /// still disagrees with (the apply path parks it again, which writes no
    /// row and leaves the reason naming the cause found now), and an op
    /// that now decodes but is refused: its bytes verified once and advanced
    /// the cursor, so keeping them is the only answer that loses nothing. Its
    /// reason becomes [`ParkReason::ReplayRefused`], because its kind is no
    /// longer unknown. A refusal is logged.
    ///
    /// A storage failure while applying or re-stamping one is not a refusal,
    /// and it is not fatal either. It is logged, that op keeps its old stamp so
    /// the next open tries it again, and the replay goes on to the next op. A
    /// fault that recurs on every try therefore costs one failed apply per
    /// open, and never stops the vault opening.
    ///
    /// Infallible for the same reason: a failure to read `parked_ops` is
    /// logged, nothing is replayed, and the next open reads it again.
    pub fn replay_parked_ops(&self, db: &mut Db) -> Vec<DomainEvent> {
        let parked = match OpLog::parked_for_replay(db, DOC_SCHEMA_V) {
            Ok(parked) => parked,
            Err(e) => {
                log_replay_failed(None, "read", &EngineError::from(e));
                return Vec::new();
            }
        };
        let mut events = Vec::new();
        for op in parked {
            let header = || decode_envelope(&op.envelope).ok();
            let reason = match self.apply_remote_all(db, &op.envelope) {
                Ok(more) => {
                    events.extend(more);
                    None
                }
                // A storage failure says nothing about the op. Re-stamping it
                // would keep an op this build can apply parked until the next
                // `DOC_SCHEMA_V` bump, so the stamp stays as it was and the
                // next open tries it again.
                Err(
                    e @ (EngineError::Storage(_) | EngineError::Sqlite(_) | EngineError::OpLog(_)),
                ) => {
                    log_replay_failed(header().as_ref(), "apply", &e);
                    continue;
                }
                // A class and not the message: a decode error can quote the
                // payload it failed on, and this payload is plaintext.
                Err(e) => {
                    let header = header();
                    tracing::warn!(
                        ev = "core.op.parked_replay_refused",
                        stream_h = header
                            .as_ref()
                            .map(|h| hex_short(&h.stream_id))
                            .unwrap_or_default(),
                        sender_h = header
                            .as_ref()
                            .map(|h| hex_short(&h.device_id))
                            .unwrap_or_default(),
                        seq = header.as_ref().map_or(0, |h| h.seq),
                        cause = match e {
                            EngineError::RemoteOpInvalid(_) => "remote_op_invalid",
                            EngineError::UnknownDevice => "unknown_device",
                            _ => "other",
                        },
                        "a parked op still does not apply; it stays parked"
                    );
                    Some(ParkReason::ReplayRefused)
                }
            };
            if let Err(e) = db.with_tx(|tx| {
                OpLog::restamp_parked(tx, &op.op_id, DOC_SCHEMA_V, reason).map_err(oplog_to_sqlite)
            }) {
                log_replay_failed(header().as_ref(), "restamp", &EngineError::from(e));
            }
        }
        events
    }
}
