//! The fourth run of [`SEED`](super::SEED): the merge state, the chain
//! bookkeeping beside it, and the compaction and snapshot rows that follow.

use rusqlite::types::ValueRef::{Blob as B, Integer as I, Text as T};

use super::super::contents::{
    CONTEXT_ERRANDS, DEVICE_LAPTOP, DEVICE_RETIRED, OP_ENVELOPE, OP_ID, STREAM_ALPHA, TASK_IN_ALPHA,
};
use super::{SeedRow, HLC_MS, TASK_BLOCKER};

pub(super) const ROWS: &[SeedRow] = &[
    SeedRow {
        table: "merge_entities",
        values: &[
            ("entity_id", B(&TASK_IN_ALPHA)),
            ("kind", T(b"task")),
            ("created", I(1)),
            ("create_hlc_ms", I(HLC_MS + 40)),
            ("create_hlc_logical", I(241)),
            ("create_device", B(&DEVICE_LAPTOP)),
            ("create_seq", I(242)),
            ("create_stream", B(&STREAM_ALPHA)),
            ("legacy_hlc_ms", I(HLC_MS + 41)),
            ("legacy_hlc_logical", I(243)),
            ("legacy_device", B(&DEVICE_LAPTOP)),
            ("legacy_seq", I(244)),
            ("legacy_stream", B(&STREAM_ALPHA)),
            ("patch_ms", I(HLC_MS + 42)),
            ("head_hlc_ms", I(HLC_MS + 43)),
            ("head_hlc_logical", I(245)),
            ("head_device", B(&DEVICE_LAPTOP)),
            ("head_seq", I(246)),
            ("head_stream", B(&STREAM_ALPHA)),
            ("row_hlc_ms", I(HLC_MS + 44)),
            ("row_hlc_logical", I(247)),
            ("row_device", B(&DEVICE_LAPTOP)),
            ("row_seq", I(248)),
        ],
    },
    SeedRow {
        table: "merge_registers",
        values: &[
            ("entity_id", B(&TASK_IN_ALPHA)),
            ("field", T(b"title")),
            // CBOR text "seeded".
            ("value", B(&[0x66, b's', b'e', b'e', b'd', b'e', b'd'])),
            ("hlc_ms", I(HLC_MS + 45)),
            ("hlc_logical", I(249)),
            ("device", B(&DEVICE_LAPTOP)),
            ("seq", I(250)),
            ("stream", B(&STREAM_ALPHA)),
            ("origin", T(b"user")),
        ],
    },
    SeedRow {
        table: "merge_map_entries",
        values: &[
            ("entity_id", B(&TASK_IN_ALPHA)),
            ("field", T(b"x_future_map")),
            ("map_key", T(b"k")),
            // CBOR unsigned 7.
            ("value", B(&[0x07])),
            ("hlc_ms", I(HLC_MS + 46)),
            ("hlc_logical", I(251)),
            ("device", B(&DEVICE_LAPTOP)),
            ("seq", I(252)),
            ("stream", B(&STREAM_ALPHA)),
            ("origin", T(b"generated")),
        ],
    },
    SeedRow {
        table: "merge_orset_adds",
        values: &[
            ("entity_id", B(&TASK_IN_ALPHA)),
            ("field", T(b"blocked_by")),
            ("element", B(&TASK_BLOCKER)),
            ("tag_stream", B(&STREAM_ALPHA)),
            ("tag_device", B(&DEVICE_LAPTOP)),
            ("tag_seq", I(253)),
            ("hlc_ms", I(HLC_MS + 47)),
            ("hlc_logical", I(254)),
        ],
    },
    SeedRow {
        table: "merge_orset_removes",
        values: &[
            ("entity_id", B(&TASK_IN_ALPHA)),
            ("field", T(b"contexts")),
            ("element", B(&CONTEXT_ERRANDS)),
            ("tag_stream", B(&STREAM_ALPHA)),
            ("tag_device", B(&DEVICE_LAPTOP)),
            ("tag_seq", I(255)),
        ],
    },
    SeedRow {
        table: "merge_counter_deltas",
        values: &[
            ("entity_id", B(&TASK_IN_ALPHA)),
            ("field", T(b"deferred_count")),
            ("op_stream", B(&STREAM_ALPHA)),
            ("op_device", B(&DEVICE_LAPTOP)),
            ("op_seq", I(256)),
            ("hlc_ms", I(HLC_MS + 48)),
            ("hlc_logical", I(257)),
            ("delta", I(-1)),
        ],
    },
    SeedRow {
        table: "chain_heads_sent",
        values: &[
            ("stream_id", B(&STREAM_ALPHA)),
            ("device_id", B(&DEVICE_RETIRED)),
            ("seq", I(258)),
        ],
    },
    SeedRow {
        table: "chain_expected",
        values: &[
            ("stream_id", B(&STREAM_ALPHA)),
            ("device_id", B(&DEVICE_RETIRED)),
            ("seq", I(259)),
            ("op_hash", B(&[0x63; 32])),
            ("named_by", B(&OP_ID)),
        ],
    },
    SeedRow {
        table: "chain_claims",
        values: &[
            ("stream_id", B(&STREAM_ALPHA)),
            ("device_id", B(&DEVICE_RETIRED)),
            ("claimed_by", B(&DEVICE_LAPTOP)),
            ("seq", I(260)),
            ("root", B(&[0x64; 32])),
        ],
    },
    SeedRow {
        table: "fork_evidence",
        values: &[
            ("stream_id", B(&STREAM_ALPHA)),
            ("device_id", B(&DEVICE_LAPTOP)),
            ("seq", I(1)),
            ("kind", T(b"seq")),
            ("held_hash", B(&[0x61; 32])),
            ("other_hash", B(&[0x65; 32])),
            ("other_envelope", B(OP_ENVELOPE)),
            ("recorded_at_ms", I(261)),
        ],
    },
    SeedRow {
        table: "chain_divergence",
        values: &[
            ("stream_id", B(&STREAM_ALPHA)),
            ("device_id", B(&DEVICE_LAPTOP)),
            ("peer_device_id", B(&DEVICE_RETIRED)),
            ("seq", I(1)),
            ("peer_root", B(&[0x66; 32])),
            ("held_root", B(&[0x62; 32])),
            ("recorded_at_ms", I(262)),
        ],
    },
    SeedRow {
        table: "compaction_floor",
        values: &[
            ("stream_id", B(&STREAM_ALPHA)),
            ("device_id", B(&DEVICE_RETIRED)),
            ("seq", I(263)),
            ("op_hash", B(&[0x67; 32])),
            ("root", B(&[0x68; 32])),
            ("hlc_ms", I(HLC_MS + 50)),
            ("hlc_logical", I(267)),
        ],
    },
    SeedRow {
        table: "peer_frontiers",
        values: &[
            ("stream_id", B(&STREAM_ALPHA)),
            ("peer_device_id", B(&DEVICE_RETIRED)),
            ("device_id", B(&DEVICE_LAPTOP)),
            ("seq", I(264)),
            ("recorded_at_ms", I(265)),
        ],
    },
    SeedRow {
        table: "stream_snapshots",
        values: &[
            ("stream_id", B(&STREAM_ALPHA)),
            ("generated_by", B(&DEVICE_LAPTOP)),
            ("generated_at_ms", I(266)),
            ("digest", B(&[0x69; 32])),
            ("record", B(b"SR\x04\x00\x01snapshot-record")),
        ],
    },
];
