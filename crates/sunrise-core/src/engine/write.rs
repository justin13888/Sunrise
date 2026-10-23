//! The one local write path (ADR-0065 §5): a command builds its op, appends
//! it to the op log, and folds it through the step a received op takes.
//!
//! [`Engine::append_and_fold`] is that path. It seals the op into the log
//! ([`Engine::ops_insert_at`]) and then hands it to
//! [`materialize_remote`], the dispatcher [`Engine::apply_remote_all`] and
//! [`Engine::rebuild_projection`] fold every received and replayed entity op
//! through. No command writes an entity row: the row writers are private to
//! [`super::merge`], so a command that tried would not compile.
//!
//! What this buys is that the writer and every peer derive a row from one
//! function of the same ops. A command reads the projection to build its op,
//! but the row it leaves behind is the fold's, not its own copy of what it
//! meant to write, so there is no second writer for the two to disagree
//! through, and a rebuild of the log lands on the rows the commands left.
//!
//! The ops are still full-state (ADR-0065 §7, plan decision D9): the
//! `Patch` builders arrive with `core.field_ops`.
//!
//! [`materialize_remote`]: super::lww::materialize_remote

use super::lww::materialize_remote;
use super::Engine;
use crate::inner_op::{encode_inner_op, InnerOp};
use rusqlite::Transaction;

/// What [`Engine::append_and_fold`] logged.
#[derive(Debug, Clone, Copy)]
pub(super) struct Appended {
    /// The op's id in `ops`.
    pub(super) op_id: [u8; 16],
    /// Its `seq` in `(stream, this device)`.
    pub(super) seq: u64,
}

impl Engine {
    /// Append `op` to this device's log under `stream`, and fold it.
    ///
    /// Inside `tx`, in the order that is safe ([`Self::log_slot`]): the
    /// stream's epoch is resolved first, which can mint one and log the
    /// `key_envelope` ops that share it, then the `seq` is read, then the op
    /// is stamped once. The op-log `inner_kind` and `target_kind` are read
    /// off the op, as the receive path reads them, so the two logs of one op
    /// agree.
    ///
    /// The fold is [`materialize_remote`] with the stamp the envelope
    /// carries, the call a peer makes when it receives the envelope.
    pub(super) fn append_and_fold(
        &self,
        tx: &Transaction<'_>,
        op: &InnerOp,
        stream: &[u8; 16],
        now_ms: u64,
    ) -> rusqlite::Result<Appended> {
        debug_assert!(!op.is_control(), "a control op has no fold");
        let inner = encode_inner_op(op)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let slot = self.log_slot(tx, stream, now_ms)?;
        let (seq, lww) = (slot.seq, slot.lww);
        let op_id = self.fresh_op_id(now_ms);
        self.ops_insert_at(
            tx,
            &op_id,
            stream,
            seq,
            lww.hlc,
            &inner,
            op.inner_kind(),
            op.target_kind(),
            Some(op.target_ref().bytes()),
            Some(now_ms),
            None,
            now_ms,
            &[],
            slot.epoch,
            &slot.key,
        )?;
        materialize_remote(tx, op, &lww, stream)?;
        Ok(Appended { op_id, seq })
    }
}
