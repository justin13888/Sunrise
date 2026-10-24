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
//! A command whose writes are only correct together appends them as one
//! group ([`Engine::append_and_fold_group`], §6), which every replica folds
//! whole or not at all (`engine::groups`).
//!
//! [`materialize_remote`]: super::lww::materialize_remote

use super::groups::{mark, record_local, Group};
use super::lww::{materialize_member, materialize_remote, LwwStamp};
use super::merge::OpRef;
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
        let (done, lww) = self.append(tx, op, stream, now_ms)?;
        materialize_remote(tx, op, &lww, stream)?;
        Ok(done)
    }

    /// Append `members` as one group (ADR-0065 §6) and fold every one of
    /// them, in this transaction: the writes of a command that are only
    /// correct together. Each member is `(op, stream)`, sealed in order.
    ///
    /// The group's id is drawn from the injected rng and its size is the
    /// number of members. A receiver holds each member back until it holds
    /// them all (`engine::groups`), so no replica shows part of the commit.
    /// One member is no group, and is appended as [`Self::append_and_fold`]
    /// appends it.
    pub(super) fn append_and_fold_group(
        &self,
        tx: &Transaction<'_>,
        members: Vec<(InnerOp, [u8; 16])>,
        now_ms: u64,
    ) -> rusqlite::Result<Vec<Appended>> {
        if members.len() < 2 {
            return members
                .iter()
                .map(|(op, stream)| self.append_and_fold(tx, op, stream, now_ms))
                .collect();
        }
        let mut id = [0u8; 16];
        self.rng.fill_bytes(&mut id);
        let group = Group {
            id,
            size: members.len() as u64,
        };
        let mut logged = Vec::with_capacity(members.len());
        for (mut op, stream) in members {
            debug_assert!(!op.is_control(), "a control op has no fold");
            // The slot is taken first: the mark names the member's own
            // op-ref, which is not known until its `seq` is.
            let slot = self.log_slot(tx, &stream, now_ms)?;
            let at = OpRef {
                stream,
                device: slot.lww.device,
                seq: slot.seq,
            };
            mark(&mut op, group, at);
            let done = self.seal(tx, &op, &stream, now_ms, slot)?;
            record_local(tx, group, at, done.1.hlc, now_ms)?;
            logged.push((op, stream, done));
        }
        for (op, stream, done) in &logged {
            materialize_member(tx, op, &done.1, stream, Some(group.pair()))?;
        }
        Ok(logged.into_iter().map(|(.., (done, _))| done).collect())
    }

    /// Take a slot in `stream` and seal `op` into it.
    fn append(
        &self,
        tx: &Transaction<'_>,
        op: &InnerOp,
        stream: &[u8; 16],
        now_ms: u64,
    ) -> rusqlite::Result<(Appended, LwwStamp)> {
        debug_assert!(!op.is_control(), "a control op has no fold");
        let slot = self.log_slot(tx, stream, now_ms)?;
        self.seal(tx, op, stream, now_ms, slot)
    }

    /// Seal `op` into the log at `slot`.
    fn seal(
        &self,
        tx: &Transaction<'_>,
        op: &InnerOp,
        stream: &[u8; 16],
        now_ms: u64,
        slot: super::oplog::LogSlot,
    ) -> rusqlite::Result<(Appended, LwwStamp)> {
        let inner = encode_inner_op(op)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
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
        Ok((Appended { op_id, seq }, lww))
    }
}
