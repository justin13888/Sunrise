//! The last run of [`SEED`](super::SEED): what the vault keeps about itself:
//! features, preferences, the blob cache, entity versions, projection and
//! full-text-index state.

use rusqlite::types::ValueRef::{Blob as B, Integer as I, Text as T};

use super::super::contents::{
    ALPHA_TASK_TITLE, DEVICE_LAPTOP, DEVICE_RETIRED, OP_ID, STREAM_ALPHA, STREAM_BETA,
    TASK_IN_ALPHA,
};
use super::{SeedRow, BLOB_ID, HLC_MS};

const GROUP_ID: [u8; 16] = [0x2b; 16];

/// CBOR `[[h'01' * 16, h'02' * 16, 7]]`: a version's one parent op-ref.
const VERSION_PARENTS: [u8; 37] = {
    let mut b = [0u8; 37];
    b[0] = 0x81;
    b[1] = 0x83;
    b[2] = 0x50;
    b[19] = 0x50;
    b[36] = 0x07;
    let mut i = 0;
    while i < 16 {
        b[3 + i] = 0x01;
        b[20 + i] = 0x02;
        i += 1;
    }
    b
};

pub(super) const ROWS: &[SeedRow] = &[
    SeedRow {
        table: "vault_required_features",
        values: &[
            ("feature", T(b"focus_session.breaks")),
            ("recorded_at_ms", I(268)),
        ],
    },
    // Two ids, newline-joined and sorted, as the fold writes them, so a
    // migration that split or trimmed the list would change the row.
    SeedRow {
        table: "device_features",
        values: &[
            ("device_id", B(&DEVICE_LAPTOP)),
            ("features", T(b"core.field_ops\nfocus_session.breaks")),
            ("hlc_ms", I(HLC_MS + 51)),
            ("hlc_logical", I(269)),
            ("recorded_at_ms", I(270)),
        ],
    },
    // The one Preferences row, under the zero id, with a CBOR map holding a
    // key this build knows and one it does not.
    SeedRow {
        table: "preferences",
        values: &[
            ("id", B(&[0u8; 16])),
            (
                "values_cbor",
                B(b"\xa2\x6afuture.key\x01\x6aweek_start\x62MO"),
            ),
            ("created_at_ms", I(271)),
            ("updated_at_ms", I(272)),
            ("lww_hlc_ms", I(HLC_MS + 52)),
            ("lww_hlc_logical", I(273)),
            ("lww_seq", I(274)),
            ("lww_device", B(&DEVICE_LAPTOP)),
            ("extra", B(b"\xa1\x63new\x01")),
        ],
    },
    SeedRow {
        table: "device_preferences",
        values: &[("key", T(b"keyboard.vim_mode")), ("value", B(b"\xf5"))],
    },
    // One indexed blob: the seed attachment's original, opened once.
    SeedRow {
        table: "blob_cache",
        values: &[
            ("blob_id", B(&BLOB_ID)),
            ("sealed_bytes", I(40_992)),
            ("last_access_ms", I(275)),
            ("is_thumbnail", I(0)),
            ("evicted_at_ms", I(276)),
        ],
    },
    // The launch pass that indexes blobs from before the index has run.
    SeedRow {
        table: "blob_cache_backfill",
        values: &[("id", I(1)), ("done_at_ms", I(277))],
    },
    // One version of the seed task: a generated create in a group, naming
    // one parent, and the entity's head.
    SeedRow {
        table: "entity_versions",
        values: &[
            ("entity_id", B(&TASK_IN_ALPHA)),
            ("stream", B(&STREAM_ALPHA)),
            ("device", B(&DEVICE_LAPTOP)),
            ("seq", I(278)),
            ("hlc_ms", I(HLC_MS + 50)),
            ("hlc_logical", I(279)),
            ("kind", T(b"patch")),
            ("origin", T(b"generated")),
            ("is_create", I(1)),
            ("parents", B(&VERSION_PARENTS)),
            ("group_id", B(&GROUP_ID)),
            ("group_size", I(2)),
            // CBOR `["title"]`.
            ("fields", B(b"\x81\x65title")),
            ("is_head", I(1)),
        ],
    },
    // One entry of that version's causal frontier.
    SeedRow {
        table: "entity_frontier",
        values: &[
            ("entity_id", B(&TASK_IN_ALPHA)),
            ("v_stream", B(&STREAM_ALPHA)),
            ("v_device", B(&DEVICE_LAPTOP)),
            ("v_seq", I(278)),
            ("device", B(&DEVICE_RETIRED)),
            ("hlc_ms", I(HLC_MS + 51)),
            ("hlc_logical", I(280)),
            ("seq", I(281)),
            ("stream", B(&STREAM_BETA)),
        ],
    },
    // A concurrent title the register keeps beside its winner.
    SeedRow {
        table: "register_losers",
        values: &[
            ("entity_id", B(&TASK_IN_ALPHA)),
            ("field", T(b"title")),
            ("stream", B(&STREAM_ALPHA)),
            ("device", B(&DEVICE_RETIRED)),
            ("seq", I(282)),
            ("hlc_ms", I(HLC_MS + 52)),
            ("hlc_logical", I(283)),
            // CBOR text "lost".
            ("value", B(b"\x64lost")),
            ("origin", T(b"user")),
            ("legacy", I(1)),
        ],
    },
    // A group member, applied.
    SeedRow {
        table: "pending_groups",
        values: &[
            ("group_id", B(&GROUP_ID)),
            ("group_size", I(3)),
            ("op_id", B(&OP_ID)),
            ("hlc_ms", I(HLC_MS + 53)),
            ("hlc_logical", I(284)),
            ("device", B(&DEVICE_LAPTOP)),
            ("seq", I(285)),
            ("stream", B(&STREAM_ALPHA)),
            ("applied", I(1)),
            ("received_at_ms", I(286)),
        ],
    },
    // The last projection rebuild.
    SeedRow {
        table: "projection_rebuild",
        values: &[("id", I(1)), ("fold_v", I(287)), ("done_at_ms", I(288))],
    },
    // The seed task in both full-text indexes (0040), under one doc: the
    // first row of each index, so rowid 1.
    SeedRow {
        table: "search_docs",
        values: &[
            ("doc", I(1)),
            ("kind", T(b"task")),
            ("id", B(&TASK_IN_ALPHA)),
        ],
    },
    SeedRow {
        table: "search_words",
        values: &[
            ("title", T(ALPHA_TASK_TITLE.as_bytes())),
            ("aux", T(b"Errands")),
            ("body", T(b"block grammar")),
        ],
    },
    SeedRow {
        table: "search_tri",
        values: &[
            ("title", T(b"ship the migration fixture")),
            ("body", T(b"block grammar")),
        ],
    },
    // How they were built, mid-way through a re-index.
    SeedRow {
        table: "search_meta",
        values: &[
            ("id", I(1)),
            ("index_v", I(290)),
            ("stemmed", I(1)),
            ("language", T(b"en-CA")),
            ("reindex_stemmed", I(0)),
            ("reindex_cursor", I(291)),
        ],
    },
];
