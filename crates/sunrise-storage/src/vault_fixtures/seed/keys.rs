//! The third run of [`SEED`](super::SEED): devices, identity and stream keys,
//! their revocations, and the blob-transfer and parked-op rows foreign-key
//! order leaves among them.

use rusqlite::types::ValueRef::{Blob as B, Integer as I, Text as T};

use super::super::contents::{
    ALPHA_KEY_ID, DEVICE_CERT_BLOB, DEVICE_LAPTOP, DEVICE_RETIRED, DH_SECRET_WRAPPED, IDENTITY_ID,
    ID_D_PRIV_WRAPPED, ID_D_PUB, ID_S_PRIV_WRAPPED, ID_S_PUB, META_KEY_ID, RETIRED_AT_MS,
    SIGNING_SECRET_WRAPPED, STREAM_ALPHA, STREAM_BETA, VAULT_META_STREAM, WRAPPED_STREAM_KEY,
};
use super::{SeedRow, ATTACHMENT, BLOB_ID, HLC_MS};

const DEFERRED_OP: [u8; 16] = [0x0d; 16];
const PARKED_OP: [u8; 16] = [0x0e; 16];
const NEXT_IDENTITY: [u8; 16] = [0x2a; 16];

pub(super) const ROWS: &[SeedRow] = &[
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
];
