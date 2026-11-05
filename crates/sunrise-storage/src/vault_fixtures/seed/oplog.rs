//! The second run of [`SEED`](super::SEED): the op log and what is queued or
//! tracked against it.

use rusqlite::types::ValueRef::{Blob as B, Integer as I, Text as T};

use super::super::contents::{DEVICE_LAPTOP, OP_ENVELOPE, OP_ID, STREAM_ALPHA, TASK_IN_ALPHA};
use super::SeedRow;

const OP_DEP: [u8; 16] = [0x0c; 16];

pub(super) const ROWS: &[SeedRow] = &[
    SeedRow {
        table: "ops",
        values: &[
            ("op_id", B(&OP_ID)),
            ("stream_id", B(&STREAM_ALPHA)),
            ("device_id", B(&DEVICE_LAPTOP)),
            ("seq", I(1)),
            ("ts_ms", I(111)),
            ("envelope", B(OP_ENVELOPE)),
            ("inner_kind", T(b"task_create")),
            ("target_kind", T(b"task")),
            ("target_id", B(&TASK_IN_ALPHA)),
            ("applied_at", I(112)),
            ("received_from", B(&DEVICE_LAPTOP)),
            ("received_at", I(113)),
            ("op_hash", B(&[0x61; 32])),
            ("chain_root", B(&[0x62; 32])),
        ],
    },
    SeedRow {
        table: "op_dep",
        values: &[("op_id", B(&OP_ID)), ("dep_id", B(&OP_DEP))],
    },
    SeedRow {
        table: "outbox",
        values: &[
            ("op_id", B(&OP_ID)),
            ("stream_id", B(&STREAM_ALPHA)),
            ("enqueued_at_ms", I(121)),
            ("acked_at_ms", I(122)),
        ],
    },
    SeedRow {
        table: "sync_cursors",
        values: &[
            ("stream_id", B(&STREAM_ALPHA)),
            ("device_id", B(&DEVICE_LAPTOP)),
            ("last_applied_seq", I(1)),
        ],
    },
];
