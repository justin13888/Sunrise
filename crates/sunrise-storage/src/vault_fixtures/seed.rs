//! The rows every fixture after v17 is filled with.
//!
//! One list for every version. [`seed_every_table`] inserts each row into its
//! table if the table exists at the fixture's version, with only the columns
//! the table has at that version. So the v14 fixture gets the pre-0017
//! `stream_keys` shape and no `identity` row, and the v31 fixture gets every
//! table this list names.
//!
//! The values are chosen to be told apart, not to be realistic. Nearly every
//! column holds a non-default value, because a migration that rewrote a column
//! to its default would go unnoticed on a row that already held the default.
//! Ids agree across tables wherever a foreign key or a reader joins them: the
//! task is in the stream, the block holds the task, the outbox row names the
//! op.
//!
//! `the_seed_reaches_every_table_and_column_this_build_creates` fails when a
//! migration adds a table this list does not fill, or when a column named here
//! exists in no version, which is what a typo would otherwise do silently.

use rusqlite::types::ToSqlOutput;
use rusqlite::types::ValueRef::{self, Blob as B, Integer as I, Text as T};
use rusqlite::Connection;

use super::{
    ALPHA_KEY_ID, ALPHA_TASK_BODY, ALPHA_TASK_TITLE, CONTEXT_ERRANDS, DEVICE_CERT_BLOB,
    DEVICE_LAPTOP, DEVICE_RETIRED, DH_SECRET_WRAPPED, IDENTITY_ID, ID_D_PRIV_WRAPPED, ID_D_PUB,
    ID_S_PRIV_WRAPPED, ID_S_PUB, META_KEY_ID, OP_ENVELOPE, OP_ID, RETIRED_AT_MS,
    SIGNING_SECRET_WRAPPED, STREAM_ALPHA, STREAM_BETA, TASK_IN_ALPHA, VAULT_META_STREAM,
    WRAPPED_STREAM_KEY,
};

const TASK_BLOCKER: [u8; 16] = [0x03; 16];
const ROUTINE: [u8; 16] = [0x04; 16];
const BLOCK: [u8; 16] = [0x05; 16];
const FOCUS_SESSION: [u8; 16] = [0x06; 16];
const REVIEW: [u8; 16] = [0x07; 16];
const NOTE: [u8; 16] = [0x08; 16];
const ATTACHMENT: [u8; 16] = [0x09; 16];
const PERSON: [u8; 16] = [0x0a; 16];
const BLOB_ID: [u8; 16] = [0x0b; 16];
const OP_DEP: [u8; 16] = [0x0c; 16];
const DEFERRED_OP: [u8; 16] = [0x0d; 16];
const PARKED_OP: [u8; 16] = [0x0e; 16];
const NEXT_IDENTITY: [u8; 16] = [0x2a; 16];

/// A stamp one millisecond into each row's own range, so no two columns
/// share a value by accident.
const HLC_MS: i64 = 1_700_000_000_001;

/// One row: its table, and a value for each column it names.
pub(super) struct SeedRow {
    pub(super) table: &'static str,
    pub(super) values: &'static [(&'static str, ValueRef<'static>)],
}

/// Every row, in insertion order: a row comes after the rows its foreign keys
/// name.
pub(super) const SEED: &[SeedRow] = &[
    SeedRow {
        table: "streams",
        values: &[
            ("stream_id", B(&STREAM_ALPHA)),
            ("head_root", B(b"\x31stream-head-root")),
            ("last_op_seq", I(3)),
            ("name", T(b"Alpha")),
            ("color", T(b"amber")),
            ("icon", T(b"star")),
            ("archived", I(1)),
            ("deleted", I(0)),
            ("paused", I(1)),
            ("paused_until_ms", I(1_700_000_100_000)),
            ("review_cadence", T(b"monthly")),
            ("reminder_lead_s", I(900)),
            ("created_at_ms", I(11)),
            ("updated_at_ms", I(12)),
            ("lww_hlc_ms", I(HLC_MS)),
            ("lww_hlc_logical", I(13)),
            ("lww_seq", I(14)),
            ("lww_device", B(&DEVICE_LAPTOP)),
            ("sort_order", T(b"AABN")),
            ("extra", B(b"\xa1\x01\x41s")),
            ("description", B(b"\x82\x01\x64desc")),
            ("default_context", B(&CONTEXT_ERRANDS)),
        ],
    },
    SeedRow {
        table: "contexts",
        values: &[
            ("id", B(&CONTEXT_ERRANDS)),
            ("name", T(b"Errands")),
            ("description", T(b"Out and about")),
            ("archived", I(1)),
            ("deleted", I(0)),
            ("created_at_ms", I(21)),
            ("updated_at_ms", I(22)),
            ("lww_hlc_ms", I(HLC_MS + 1)),
            ("lww_hlc_logical", I(23)),
            ("lww_seq", I(24)),
            ("lww_device", B(&DEVICE_LAPTOP)),
            ("extra", B(b"\xa1\x01\x41c")),
        ],
    },
    SeedRow {
        table: "routines",
        values: &[
            ("id", B(&ROUTINE)),
            ("stream_id", B(&STREAM_ALPHA)),
            ("rrule_text", T(b"FREQ=WEEKLY;BYDAY=MO")),
            ("timezone", T(b"Europe/Paris")),
            ("starts_at_ms", I(1_700_000_200_000)),
            ("ends_at_ms", I(1_800_000_000_000)),
            ("template", B(b"\xa1\x01\x48template")),
            ("skip_dates", B(b"\x81\x1a\x00\x01\x02\x03")),
            ("skipped_keys", B(b"\x81\x63key")),
            ("catchup_policy", T(b"one")),
            ("streak_counter", I(4)),
            ("streak_state", B(b"\xa1\x01\x02")),
            ("last_completed_at_ms", I(1_700_000_300_000)),
            ("materialized_until_ms", I(1_700_000_400_000)),
            ("paused", I(1)),
            ("paused_until_ms", I(1_700_000_500_000)),
            ("archived", I(1)),
            ("deleted", I(0)),
            ("scheduling_constraints", B(b"\xa1\x02\x03")),
            ("created_at_ms", I(31)),
            ("updated_at_ms", I(32)),
            ("lww_hlc_ms", I(HLC_MS + 2)),
            ("lww_hlc_logical", I(33)),
            ("lww_seq", I(34)),
            ("lww_device", B(&DEVICE_LAPTOP)),
            ("extra", B(b"\xa1\x01\x41r")),
            ("rrule_cbor", B(b"\xa2\x01\x02\x03\x04")),
        ],
    },
    SeedRow {
        table: "tasks",
        values: &[
            ("id", B(&TASK_IN_ALPHA)),
            ("stream_id", B(&STREAM_ALPHA)),
            ("title", T(ALPHA_TASK_TITLE.as_bytes())),
            ("state", T(b"waiting")),
            ("priority", I(2)),
            ("energy", T(b"high")),
            ("estimated_min", I(25)),
            ("scheduled_at_ms", I(1_700_000_600_000)),
            ("scheduled_at_kind", T(b"instant")),
            ("scheduled_at_tz", T(b"Europe/Paris")),
            ("due_at_ms", I(1_700_000_700_000)),
            ("due_at_kind", T(b"date")),
            ("due_at_tz", T(b"UTC")),
            ("completed_at_ms", I(1_700_000_800_000)),
            ("completed_at_kind", T(b"instant")),
            ("completed_at_tz", T(b"Asia/Tokyo")),
            ("deferred_count", I(5)),
            ("routine_id", B(&ROUTINE)),
            ("routine_occurrence", I(7)),
            ("reminder_lead_s", I(300)),
            ("archived", I(1)),
            ("deleted", I(0)),
            ("body", B(ALPHA_TASK_BODY)),
            ("extra", B(b"\xa1\x01\x41t")),
            ("head_root", B(b"\x32task-head-root")),
            ("scheduling_constraints", B(b"\xa1\x04\x05")),
            ("created_at_ms", I(41)),
            ("updated_at_ms", I(42)),
            ("lww_hlc_ms", I(HLC_MS + 3)),
            ("lww_hlc_logical", I(43)),
            ("lww_seq", I(44)),
            ("lww_device", B(&DEVICE_LAPTOP)),
        ],
    },
    SeedRow {
        table: "task_contexts",
        values: &[
            ("task_id", B(&TASK_IN_ALPHA)),
            ("context_id", B(&CONTEXT_ERRANDS)),
        ],
    },
    SeedRow {
        table: "task_blockers",
        values: &[
            ("task_id", B(&TASK_IN_ALPHA)),
            ("blocker_id", B(&TASK_BLOCKER)),
        ],
    },
    SeedRow {
        table: "blocks",
        values: &[
            ("id", B(&BLOCK)),
            ("stream_id", B(&STREAM_ALPHA)),
            ("starts_at_ms", I(1_700_000_900_000)),
            ("starts_at_kind", T(b"floating")),
            ("starts_at_tz", T(b"Europe/Paris")),
            ("ends_at_ms", I(1_700_001_000_000)),
            ("ends_at_kind", T(b"floating")),
            ("ends_at_tz", T(b"Europe/Paris")),
            ("title", T(b"Deep work")),
            ("title_track_task", I(1)),
            ("deleted", I(0)),
            ("extra", B(b"\xa1\x01\x41b")),
            ("created_at_ms", I(51)),
            ("updated_at_ms", I(52)),
            ("lww_hlc_ms", I(HLC_MS + 4)),
            ("lww_hlc_logical", I(53)),
            ("lww_seq", I(54)),
            ("lww_device", B(&DEVICE_LAPTOP)),
        ],
    },
    SeedRow {
        table: "block_tasks",
        values: &[("block_id", B(&BLOCK)), ("task_id", B(&TASK_IN_ALPHA))],
    },
    SeedRow {
        table: "focus_sessions",
        values: &[
            ("id", B(&FOCUS_SESSION)),
            ("task_id", B(&TASK_IN_ALPHA)),
            ("stream_id", B(&STREAM_ALPHA)),
            ("started_at_ms", I(1_700_001_100_000)),
            ("planned_ms", I(1_500_000)),
            ("energy", T(b"medium")),
            ("kind", T(b"work")),
            ("chunk_index", I(1)),
            ("chunk_total", I(3)),
            ("lww_hlc_ms", I(HLC_MS + 5)),
            ("lww_hlc_logical", I(63)),
            ("lww_seq", I(64)),
            ("lww_device", B(&DEVICE_LAPTOP)),
            ("extra", B(b"\xa1\x01\x41f")),
        ],
    },
    SeedRow {
        table: "focus_session_ends",
        values: &[
            ("session_id", B(&FOCUS_SESSION)),
            ("ended_at_ms", I(1_700_001_200_000)),
            ("actual_focused_ms", I(1_400_000)),
            ("completed_task", I(1)),
            ("lww_hlc_ms", I(HLC_MS + 6)),
            ("lww_hlc_logical", I(73)),
            ("lww_seq", I(74)),
            ("lww_device", B(&DEVICE_LAPTOP)),
            ("extra", B(b"\xa1\x01\x41e")),
        ],
    },
    SeedRow {
        table: "focus_interruptions",
        values: &[
            ("session_id", B(&FOCUS_SESSION)),
            ("at_ms", I(1_700_001_150_000)),
            ("reason", T(b"meeting")),
        ],
    },
    SeedRow {
        table: "review_snapshots",
        values: &[
            ("id", B(&REVIEW)),
            ("created_at_ms", I(81)),
            ("window_start_ms", I(1_699_000_000_000)),
            ("window_end_ms", I(1_699_600_000_000)),
            ("completed", I(6)),
            ("deferred", I(2)),
            ("dropped", I(1)),
            ("created", I(9)),
            ("reopened", I(3)),
            ("body", B(b"\xa1\x01\x46review")),
            ("lww_hlc_ms", I(HLC_MS + 7)),
            ("lww_hlc_logical", I(83)),
            ("lww_seq", I(84)),
            ("lww_device", B(&DEVICE_LAPTOP)),
        ],
    },
    SeedRow {
        table: "notes",
        values: &[
            ("id", B(&NOTE)),
            ("parent_kind", T(b"task")),
            ("parent_id", B(&TASK_IN_ALPHA)),
            ("body", B(b"\x82\x01\x64note")),
            ("created_at_ms", I(91)),
            ("updated_at_ms", I(92)),
            ("deleted", I(0)),
        ],
    },
    SeedRow {
        table: "attachments",
        values: &[
            ("id", B(&ATTACHMENT)),
            ("parent_kind", T(b"task")),
            ("parent_id", B(&TASK_IN_ALPHA)),
            ("filename", T(b"receipt.pdf")),
            ("mime_type", T(b"application/pdf")),
            ("size_bytes", I(40_960)),
            ("blob_key", B(b"\x33wrapped-blob-key")),
            ("blob_id", B(&BLOB_ID)),
            ("chunk_count", I(2)),
            ("content_hash", B(b"\x34content-hash")),
            ("deleted", I(0)),
            ("extra", B(b"\xa1\x01\x41a")),
            ("created_at_ms", I(101)),
            ("updated_at_ms", I(102)),
            ("lww_hlc_ms", I(HLC_MS + 8)),
            ("lww_hlc_logical", I(103)),
            ("lww_seq", I(104)),
            ("lww_device", B(&DEVICE_LAPTOP)),
            ("ciphertext_hash", B(b"\x35ciphertext-hash")),
            ("width", I(4032)),
            ("height", I(3024)),
            ("thumbnail_blob_id", B(&[0x36; 16])),
            ("thumbnail_blob_key", B(&[0x37; 32])),
            ("thumbnail_mime", T(b"image/jpeg")),
            ("thumbnail_size_bytes", I(20_480)),
            ("thumbnail_content_hash", B(&[0x38; 32])),
            ("thumbnail_ciphertext_hash", B(&[0x39; 32])),
        ],
    },
    SeedRow {
        table: "persons",
        values: &[
            ("id", B(&PERSON)),
            ("display_name", T(b"Ada")),
            ("identity_id", B(&IDENTITY_ID)),
            ("deleted", I(0)),
        ],
    },
    SeedRow {
        table: "search_idx",
        values: &[
            ("kind", T(b"task")),
            ("id", B(&TASK_IN_ALPHA)),
            ("stream_id", B(&STREAM_ALPHA)),
            ("title", T(ALPHA_TASK_TITLE.as_bytes())),
            ("body", T(b"block grammar")),
            ("contexts", T(b"Errands")),
        ],
    },
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
    SeedRow {
        table: "local_identity",
        values: &[
            ("id", I(1)),
            ("device_id", B(&DEVICE_LAPTOP)),
            ("signing_secret_wrapped", B(SIGNING_SECRET_WRAPPED)),
            ("cert_blob", B(DEVICE_CERT_BLOB)),
            ("created_at_ms", I(131)),
            ("dh_secret_wrapped", B(DH_SECRET_WRAPPED)),
        ],
    },
    SeedRow {
        table: "devices",
        values: &[
            ("device_id", B(&DEVICE_LAPTOP)),
            ("cert_blob", B(DEVICE_CERT_BLOB)),
            ("nickname", T(b"laptop")),
            ("platform", T(b"macos")),
            ("created_at_ms", I(141)),
            ("identity_id", B(&IDENTITY_ID)),
            ("d_d_pub", B(&ID_D_PUB)),
            ("admitted_after_revocation", I(1)),
        ],
    },
    SeedRow {
        table: "devices",
        values: &[
            ("device_id", B(&DEVICE_RETIRED)),
            ("cert_blob", B(DEVICE_CERT_BLOB)),
            ("nickname", T(b"old-phone")),
            ("platform", T(b"ios")),
            ("created_at_ms", I(142)),
            ("identity_id", B(&IDENTITY_ID)),
            ("d_d_pub", B(&ID_D_PUB)),
            ("admitted_after_revocation", I(0)),
        ],
    },
    // The vault-meta stream's first epoch, minted here: this vault created the
    // account, so 0019 keeps `identity.id_d_priv_wrapped`. Before 0017 the
    // table has no `key_id` or `source`, and 0017 drops the row on purpose.
    SeedRow {
        table: "stream_keys",
        values: &[
            ("stream_id", B(&VAULT_META_STREAM)),
            ("epoch", I(1)),
            ("key_id", B(META_KEY_ID)),
            ("wrapped", B(WRAPPED_STREAM_KEY)),
            ("source", T(b"local")),
            ("created_at_ms", I(151)),
        ],
    },
    SeedRow {
        table: "identity",
        values: &[
            ("id", I(1)),
            ("identity_id", B(&IDENTITY_ID)),
            ("id_s_pub", B(&ID_S_PUB)),
            ("id_d_pub", B(&ID_D_PUB)),
            ("id_s_priv_wrapped", B(ID_S_PRIV_WRAPPED)),
            ("id_d_priv_wrapped", B(ID_D_PRIV_WRAPPED)),
            ("created_at_ms", I(161)),
            ("minted_by_device_id", B(&DEVICE_LAPTOP)),
            ("genesis_identity_id", B(&IDENTITY_ID)),
            ("genesis_id_s_pub", B(&ID_S_PUB)),
        ],
    },
    SeedRow {
        table: "deferred_ops",
        values: &[
            ("op_id", B(&DEFERRED_OP)),
            ("stream_id", B(&STREAM_BETA)),
            ("epoch", I(2)),
            ("envelope", B(b"\xa4\x02deferred-envelope")),
            ("received_at_ms", I(171)),
        ],
    },
    SeedRow {
        table: "device_revocations",
        values: &[
            ("device_id", B(&DEVICE_RETIRED)),
            ("cut_ms", I(RETIRED_AT_MS)),
            ("cut_logical", I(2)),
            ("revoked_by", B(&DEVICE_LAPTOP)),
            ("reason", T(b"Lost")),
            ("recorded_at_ms", I(RETIRED_AT_MS + 1)),
        ],
    },
    SeedRow {
        table: "key_envelope_recipients",
        values: &[
            ("stream_id", B(&STREAM_ALPHA)),
            ("epoch", I(1)),
            ("recipient", B(&DEVICE_LAPTOP)),
            ("recorded_at_ms", I(181)),
        ],
    },
    SeedRow {
        table: "relay_revocation_intents",
        values: &[
            ("device_id", B(&DEVICE_RETIRED)),
            ("created_at_ms", I(191)),
            ("attempts", I(2)),
            ("last_attempt_ms", I(192)),
        ],
    },
    SeedRow {
        table: "identity_transitions",
        values: &[
            ("to_identity_id", B(&NEXT_IDENTITY)),
            ("from_identity_id", B(&IDENTITY_ID)),
            ("to_id_s_pub", B(&[0x2b; 32])),
            ("to_id_d_pub", B(&[0x2c; 32])),
            ("meta_epoch", I(2)),
            ("hlc_physical_ms", I(HLC_MS + 9)),
            ("hlc_logical", I(203)),
            ("emitter_device_id", B(&DEVICE_LAPTOP)),
            ("payload", B(b"\xa1\x01\x47payload")),
            ("prev_sig", B(&[0x2d; 64])),
            ("next_sig", B(&[0x2e; 64])),
            ("roster_digest", B(&[0x2f; 32])),
            ("shares_digest", B(&[0x30; 32])),
        ],
    },
    SeedRow {
        table: "blob_uploads",
        values: &[
            ("blob_id", B(&BLOB_ID)),
            ("stream_id", B(&STREAM_ALPHA)),
            ("chunk_count", I(2)),
            ("size_bytes", I(40_976)),
            ("ciphertext_hash", B(b"\x35ciphertext-hash")),
            ("upload_id", T(b"upload-1")),
            ("attempts", I(3)),
            ("created_at_ms", I(211)),
            ("last_attempt_ms", I(212)),
        ],
    },
    SeedRow {
        table: "blob_fetches",
        values: &[
            ("attachment_id", B(&ATTACHMENT)),
            ("blob_id", B(&BLOB_ID)),
            ("state", T(b"pending")),
            ("requested_at_ms", I(221)),
            ("attempts", I(1)),
            ("last_attempt_ms", I(222)),
        ],
    },
    // What 0027 would seed from the `device_revocations` row above, so a
    // fixture written after 0027 holds the same ledger a vault that migrated
    // through it does.
    SeedRow {
        table: "device_revoke_ops",
        values: &[
            ("op_hlc_ms", I(RETIRED_AT_MS)),
            ("op_hlc_logical", I(2)),
            ("sender", B(&DEVICE_LAPTOP)),
            ("revoked_device_id", B(&DEVICE_RETIRED)),
            ("reason", T(b"Lost")),
            ("recorded_at_ms", I(RETIRED_AT_MS + 1)),
        ],
    },
    SeedRow {
        table: "device_read_bounds",
        values: &[
            ("device_id", B(&DEVICE_RETIRED)),
            ("first_bound_at_ms", I(RETIRED_AT_MS + 1)),
            ("from_sponsor", I(1)),
        ],
    },
    SeedRow {
        table: "stream_key_senders",
        values: &[
            ("stream_id", B(&STREAM_ALPHA)),
            ("epoch", I(1)),
            ("key_id", B(ALPHA_KEY_ID)),
            ("sender_device_id", B(&DEVICE_LAPTOP)),
        ],
    },
    SeedRow {
        table: "parked_ops",
        values: &[
            ("op_id", B(&PARKED_OP)),
            ("reason", T(b"unknown_kind")),
            ("kind", T(b"x_future_kind")),
            ("hlc_logical", I(233)),
            ("parked_under_doc_schema_v", I(5)),
            ("parked_at_ms", I(234)),
        ],
    },
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
];

/// The columns `table` has in `conn`'s schema, or `None` if it has no such
/// table.
pub(super) fn columns(conn: &Connection, table: &str) -> Option<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info(?)")
        .expect("prepare pragma_table_info");
    let names = stmt
        .query_map([table], |r| r.get::<_, String>(0))
        .expect("pragma_table_info")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("read pragma_table_info");
    (!names.is_empty()).then_some(names)
}

/// Fill a vault at whatever version `conn` holds from [`SEED`]: every row
/// whose table exists, with every column that exists.
pub(super) fn seed_every_table(conn: &Connection) {
    for row in SEED {
        let Some(present) = columns(conn, row.table) else {
            continue;
        };
        let values: Vec<&(&str, ValueRef<'static>)> = row
            .values
            .iter()
            .filter(|(column, _)| present.iter().any(|p| p == column))
            .collect();
        let names: Vec<&str> = values.iter().map(|(c, _)| *c).collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            row.table,
            names.join(", "),
            vec!["?"; names.len()].join(", ")
        );
        let inserted = conn
            .execute(
                &sql,
                rusqlite::params_from_iter(values.iter().map(|(_, v)| ToSqlOutput::Borrowed(*v))),
            )
            .unwrap_or_else(|e| panic!("seed `{}`: {e}", row.table));
        assert_eq!(inserted, 1, "seed `{}`", row.table);
    }
}
