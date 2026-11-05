//! The seeds of the two curated fixtures, each shaped around the migrations
//! that move data the other cannot contain.

use rusqlite::Connection;

use super::contents::{
    ALPHA_KEY_ID, ALPHA_TASK_BODY, ALPHA_TASK_TITLE, CONTEXT_ERRANDS, DEVICE_CERT_BLOB,
    DEVICE_LAPTOP, DEVICE_RETIRED, DH_SECRET_WRAPPED, IDENTITY_ID, ID_D_PRIV_WRAPPED, ID_D_PUB,
    ID_S_PRIV_WRAPPED, ID_S_PUB, INBOX_TASK_TITLE, LEGACY_INBOX, META_KEY_ID, OP_ENVELOPE, OP_ID,
    REMAPPED_INBOX, RETIRED_AT_MS, SIGNING_SECRET_WRAPPED, STREAM_ALPHA, STREAM_BETA, STREAM_GAMMA,
    TASK_IN_ALPHA, TASK_IN_INBOX, VAULT_META_STREAM, WRAPPED_STREAM_KEY,
};

/// The rows a 13-era vault holds.
///
/// Chosen so that every data-moving thing 0014 and 0017 do has something to
/// move: streams out of alphabetical order and one of them the lazily created
/// Inbox placeholder, a task parked in the legacy Inbox stream, a revoked
/// device, and a pre-hierarchy `stream_keys` row.
pub(super) fn seed_v13(conn: &Connection) {
    for (id, name) in [
        (LEGACY_INBOX, ""),
        (STREAM_GAMMA, "Gamma"),
        (STREAM_BETA, "beta"),
        (STREAM_ALPHA, "Alpha"),
    ] {
        conn.execute(
            "INSERT INTO streams
             (stream_id, head_root, last_op_seq, name, created_at_ms, updated_at_ms)
             VALUES (?, X'00', 0, ?, 1, 2)",
            rusqlite::params![id.to_vec(), name],
        )
        .expect("seed streams");
    }

    conn.execute(
        "INSERT INTO tasks (id, stream_id, title, state, created_at_ms, updated_at_ms)
         VALUES (?, ?, ?, 'open', 3, 4)",
        rusqlite::params![
            TASK_IN_INBOX.to_vec(),
            LEGACY_INBOX.to_vec(),
            INBOX_TASK_TITLE
        ],
    )
    .expect("seed the inbox task");
    conn.execute(
        "INSERT INTO tasks (id, stream_id, title, state, body, created_at_ms, updated_at_ms)
         VALUES (?, ?, ?, 'open', ?, 5, 6)",
        rusqlite::params![
            TASK_IN_ALPHA.to_vec(),
            STREAM_ALPHA.to_vec(),
            ALPHA_TASK_TITLE,
            ALPHA_TASK_BODY,
        ],
    )
    .expect("seed the alpha task");

    conn.execute(
        "INSERT INTO contexts (id, name) VALUES (?, 'Errands')",
        rusqlite::params![CONTEXT_ERRANDS.to_vec()],
    )
    .expect("seed contexts");
    conn.execute(
        "INSERT INTO task_contexts (task_id, context_id) VALUES (?, ?)",
        rusqlite::params![TASK_IN_ALPHA.to_vec(), CONTEXT_ERRANDS.to_vec()],
    )
    .expect("seed task_contexts");

    conn.execute(
        "INSERT INTO ops
         (op_id, stream_id, device_id, seq, ts_ms, envelope, inner_kind,
          target_kind, target_id, received_at)
         VALUES (?, ?, ?, 1, 7, ?, 'task_create', 'task', ?, 8)",
        rusqlite::params![
            OP_ID.to_vec(),
            STREAM_ALPHA.to_vec(),
            DEVICE_LAPTOP.to_vec(),
            OP_ENVELOPE,
            TASK_IN_ALPHA.to_vec(),
        ],
    )
    .expect("seed ops");
    conn.execute(
        "INSERT INTO outbox (op_id, stream_id, enqueued_at_ms) VALUES (?, ?, 9)",
        rusqlite::params![OP_ID.to_vec(), STREAM_ALPHA.to_vec()],
    )
    .expect("seed outbox");
    conn.execute(
        "INSERT INTO sync_cursors (stream_id, device_id, last_applied_seq)
         VALUES (?, ?, 1)",
        rusqlite::params![STREAM_ALPHA.to_vec(), DEVICE_LAPTOP.to_vec()],
    )
    .expect("seed sync_cursors");

    seed_v13_devices_and_keys(conn);
}

/// The half of a 13-era vault 0017 rewrites: the local identity, one live and
/// one revoked device, and a `stream_keys` row from the derived schedule.
fn seed_v13_devices_and_keys(conn: &Connection) {
    conn.execute(
        "INSERT INTO local_identity
         (id, device_id, signing_secret_wrapped, cert_blob, created_at_ms)
         VALUES (1, ?, ?, ?, 10)",
        rusqlite::params![
            DEVICE_LAPTOP.to_vec(),
            SIGNING_SECRET_WRAPPED,
            DEVICE_CERT_BLOB,
        ],
    )
    .expect("seed local_identity");
    conn.execute(
        "INSERT INTO devices
         (device_id, cert_blob, nickname, platform, created_at_ms, revoked_at_ms)
         VALUES (?, ?, 'laptop', 'macos', 10, NULL)",
        rusqlite::params![DEVICE_LAPTOP.to_vec(), DEVICE_CERT_BLOB],
    )
    .expect("seed the live device");
    conn.execute(
        "INSERT INTO devices
         (device_id, cert_blob, nickname, platform, created_at_ms, revoked_at_ms)
         VALUES (?, ?, 'old-phone', 'ios', 10, ?)",
        rusqlite::params![DEVICE_RETIRED.to_vec(), DEVICE_CERT_BLOB, RETIRED_AT_MS],
    )
    .expect("seed the retired device");

    conn.execute(
        "INSERT INTO stream_keys (stream_id, epoch, wrapped, created_at_ms)
         VALUES (?, 1, ?, 11)",
        rusqlite::params![STREAM_ALPHA.to_vec(), WRAPPED_STREAM_KEY],
    )
    .expect("seed the pre-hierarchy stream key");
}

/// The rows a 17-era vault holds: the same shape, already past 0017, plus the
/// account identity and the re-keyed `stream_keys` that only exist from 17 on.
///
/// `source = 'local'` on vault-meta epoch 1 makes this the account's
/// **creator**, which is the branch of 0019 that must *not* lose data. The
/// paired branch — where the column is deliberately blanked — is covered by
/// `db/tests.rs`'s `migration_0019_clears_a_paired_devices_identity_key_and_keeps_the_creators`,
/// and cannot share a fixture with this one because `identity` holds exactly
/// one row.
pub(super) fn seed_v17(conn: &Connection) {
    conn.execute(
        "INSERT INTO streams
         (stream_id, head_root, last_op_seq, name, sort_order, created_at_ms, updated_at_ms)
         VALUES (?, X'00', 0, '', 'AAAN', 1, 2)",
        rusqlite::params![REMAPPED_INBOX.to_vec()],
    )
    .expect("seed the inbox stream");
    conn.execute(
        "INSERT INTO streams
         (stream_id, head_root, last_op_seq, name, sort_order, created_at_ms, updated_at_ms)
         VALUES (?, X'00', 0, 'Alpha', 'AABN', 1, 2)",
        rusqlite::params![STREAM_ALPHA.to_vec()],
    )
    .expect("seed the alpha stream");
    conn.execute(
        "INSERT INTO tasks (id, stream_id, title, state, body, created_at_ms, updated_at_ms)
         VALUES (?, ?, ?, 'open', ?, 5, 6)",
        rusqlite::params![
            TASK_IN_ALPHA.to_vec(),
            STREAM_ALPHA.to_vec(),
            ALPHA_TASK_TITLE,
            ALPHA_TASK_BODY,
        ],
    )
    .expect("seed the alpha task");
    conn.execute(
        "INSERT INTO ops
         (op_id, stream_id, device_id, seq, ts_ms, envelope, inner_kind,
          target_kind, target_id, received_at)
         VALUES (?, ?, ?, 1, 7, ?, 'task_create', 'task', ?, 8)",
        rusqlite::params![
            OP_ID.to_vec(),
            STREAM_ALPHA.to_vec(),
            DEVICE_LAPTOP.to_vec(),
            OP_ENVELOPE,
            TASK_IN_ALPHA.to_vec(),
        ],
    )
    .expect("seed ops");

    conn.execute(
        "INSERT INTO local_identity
         (id, device_id, signing_secret_wrapped, cert_blob, dh_secret_wrapped, created_at_ms)
         VALUES (1, ?, ?, ?, ?, 10)",
        rusqlite::params![
            DEVICE_LAPTOP.to_vec(),
            SIGNING_SECRET_WRAPPED,
            DEVICE_CERT_BLOB,
            DH_SECRET_WRAPPED,
        ],
    )
    .expect("seed local_identity");
    conn.execute(
        "INSERT INTO identity
         (id, identity_id, id_s_pub, id_d_pub, id_s_priv_wrapped, id_d_priv_wrapped,
          created_at_ms)
         VALUES (1, ?, ?, ?, ?, ?, 10)",
        rusqlite::params![
            IDENTITY_ID.to_vec(),
            ID_S_PUB.to_vec(),
            ID_D_PUB.to_vec(),
            ID_S_PRIV_WRAPPED,
            ID_D_PRIV_WRAPPED,
        ],
    )
    .expect("seed identity");
    conn.execute(
        "INSERT INTO devices
         (device_id, cert_blob, nickname, platform, created_at_ms, identity_id, d_d_pub)
         VALUES (?, ?, 'laptop', 'macos', 10, ?, ?)",
        rusqlite::params![
            DEVICE_LAPTOP.to_vec(),
            DEVICE_CERT_BLOB,
            IDENTITY_ID.to_vec(),
            ID_D_PUB.to_vec(),
        ],
    )
    .expect("seed devices");

    for (stream, key_id, source) in [
        (VAULT_META_STREAM, META_KEY_ID, "local"),
        (STREAM_ALPHA, ALPHA_KEY_ID, "envelope"),
    ] {
        conn.execute(
            "INSERT INTO stream_keys
             (stream_id, epoch, key_id, wrapped, source, created_at_ms)
             VALUES (?, 1, ?, ?, ?, 11)",
            rusqlite::params![stream.to_vec(), key_id, WRAPPED_STREAM_KEY, source],
        )
        .expect("seed stream_keys");
    }
}
