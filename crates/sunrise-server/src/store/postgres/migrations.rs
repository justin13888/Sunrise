//! The Postgres backend's schema history, and the runner that applies it.
//!
//! The same rules as the SQLite history in `store/migrations`, in Postgres's
//! dialect, on a list of its own (ADR-0062 §6):
//!
//! - **Append only.** A shipped migration is never edited, reordered or
//!   removed; a schema change is a new entry with the next id.
//! - **One transaction per migration**, with the version stamp written inside
//!   it, so a crash leaves the database at the old version with the old schema
//!   or the new with the new.
//! - **One migrator at a time.** Every step first takes a transaction-scoped
//!   advisory lock, so two nodes starting together apply each step once: the
//!   second waits, re-reads the version, and finds the step applied.
//! - **A newer database is refused, never written.** A version above
//!   [`LATEST`] means a newer release migrated the database; the relay exits
//!   78 before it writes a row.
//!
//! The version lives in `schema_version`, a one-row table, because Postgres
//! has no header field like SQLite's `user_version`. The list starts at 1 with
//! the schema the SQLite history had reached at its migration 8: a Postgres
//! database is created by this release and has no older shape to adopt, so its
//! baseline is the whole schema rather than eight steps replayed. Its numbers
//! are its own; they are not SQLite's.

use tokio_postgres::Client;

use super::OpenError;

/// One step of the history.
struct Migration {
    /// The version a database is at once this step has applied.
    id: i32,
    /// What the step does, for the log line.
    name: &'static str,
    /// The schema change. Runs inside the step's transaction.
    sql: &'static str,
}

/// Every migration, in apply order, ids `1..=LATEST` with no gaps.
const MIGRATIONS: &[Migration] = &[
    Migration {
        id: 1,
        name: "baseline",
        sql: BASELINE,
    },
    Migration {
        id: 2,
        name: "devices_revoked_by_key",
        sql: DEVICES_REVOKED_BY_KEY,
    },
];

/// The version this binary migrates to, and the newest it will open.
pub(super) const LATEST: i32 = MIGRATIONS[MIGRATIONS.len() - 1].id;

/// The advisory lock every migration step takes, so two nodes never apply one
/// step twice. A constant of this module's own: the other advisory locks the
/// relay takes use other keys.
const MIGRATE_LOCK: i64 = 0x5352_5345_4d49_4752;

/// 0001: everything the relay keeps, as the SQLite history stood at its
/// migration 8, plus the two tables only a shared backend needs: the sync
/// sessions of ADR-0062 §3 and the rate cells of §5.
///
/// `relay_frames.from_conn` is the session connection that published the
/// frame, so a node that delivers it from the shared log can still drop it
/// from its author's own stream. `retired_recovery_blobs.id` is the order
/// SQLite's `rowid` gave the same table.
const BASELINE: &str = r"
CREATE TABLE accounts (
    account_id             TEXT PRIMARY KEY,
    oidc_iss               TEXT NOT NULL,
    oidc_sub               TEXT NOT NULL,
    email                  TEXT,
    identity_pub_s         TEXT,
    identity_pub_d         TEXT,
    recovery_blob          TEXT,
    tier                   TEXT NOT NULL DEFAULT 'free',
    terms_at_ms            BIGINT,
    created_at_ms          BIGINT NOT NULL,
    delete_requested_at_ms BIGINT,
    UNIQUE (oidc_iss, oidc_sub)
);
CREATE INDEX accounts_pending_deletion ON accounts (delete_requested_at_ms)
    WHERE delete_requested_at_ms IS NOT NULL;

CREATE TABLE devices (
    device_id       TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    device_pub_s    TEXT NOT NULL,
    device_pub_d    TEXT,
    device_cert     TEXT,
    vault_device_id TEXT,
    nickname        TEXT NOT NULL,
    platform        TEXT NOT NULL,
    app_version     TEXT,
    created_at_ms   BIGINT NOT NULL,
    last_seen_at_ms BIGINT NOT NULL,
    revoked         BOOLEAN NOT NULL DEFAULT FALSE,
    revoked_at_ms   BIGINT
);
CREATE INDEX devices_by_account ON devices (account_id);
CREATE INDEX devices_by_vault_id ON devices (account_id, vault_device_id);

CREATE TABLE push_tokens (
    device_id     TEXT NOT NULL REFERENCES devices (device_id) ON DELETE CASCADE,
    platform      TEXT NOT NULL,
    token         TEXT NOT NULL,
    updated_at_ms BIGINT NOT NULL,
    PRIMARY KEY (device_id, platform)
);

CREATE TABLE account_delete_tokens (
    account_id    TEXT PRIMARY KEY REFERENCES accounts (account_id) ON DELETE CASCADE,
    token_h       BYTEA NOT NULL,
    expires_at_ms BIGINT NOT NULL
);

CREATE TABLE blob_tombstones (
    account_id       TEXT NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    blob_key         BYTEA NOT NULL,
    stream_id        BYTEA NOT NULL,
    origin_device    BYTEA NOT NULL,
    seq              BIGINT NOT NULL,
    deleted_by       TEXT,
    tombstoned_at_ms BIGINT NOT NULL,
    PRIMARY KEY (account_id, blob_key)
);
CREATE INDEX blob_tombstones_by_age ON blob_tombstones (tombstoned_at_ms);

CREATE TABLE device_cursors (
    device_id      TEXT NOT NULL REFERENCES devices (device_id) ON DELETE CASCADE,
    stream_id      BYTEA NOT NULL,
    origin_device  BYTEA NOT NULL,
    applied_seq    BIGINT NOT NULL,
    reported_at_ms BIGINT NOT NULL,
    PRIMARY KEY (device_id, stream_id, origin_device)
);

CREATE TABLE committed_blobs (
    account_id TEXT NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    blob_key   BYTEA NOT NULL,
    size_bytes BIGINT NOT NULL,
    PRIMARY KEY (account_id, blob_key)
);

CREATE TABLE stream_snapshots (
    account_id      TEXT NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    stream_id       BYTEA NOT NULL,
    blob_key        BYTEA NOT NULL,
    size_bytes      BIGINT NOT NULL,
    published_by    TEXT,
    published_at_ms BIGINT NOT NULL,
    PRIMARY KEY (account_id, stream_id)
);

CREATE TABLE key_bundles (
    account_id      TEXT PRIMARY KEY REFERENCES accounts (account_id) ON DELETE CASCADE,
    blob_key        BYTEA NOT NULL,
    size_bytes      BIGINT NOT NULL,
    manifest_digest BYTEA NOT NULL,
    published_by    TEXT,
    published_at_ms BIGINT NOT NULL
);

CREATE TABLE identity_chains (
    account_id      TEXT PRIMARY KEY REFERENCES accounts (account_id) ON DELETE CASCADE,
    chain           BYTEA NOT NULL,
    digest          BYTEA NOT NULL,
    published_by    TEXT,
    published_at_ms BIGINT NOT NULL
);

CREATE TABLE retired_recovery_blobs (
    id            BIGSERIAL PRIMARY KEY,
    account_id    TEXT NOT NULL REFERENCES accounts (account_id) ON DELETE CASCADE,
    retired_at_ms BIGINT NOT NULL,
    check_prefix  BYTEA NOT NULL
);
CREATE INDEX retired_recovery_blobs_by_account
    ON retired_recovery_blobs (account_id, retired_at_ms);

CREATE TABLE relay_frames (
    id         BIGSERIAL PRIMARY KEY,
    account_h  BYTEA NOT NULL,
    stream_id  BYTEA NOT NULL,
    bytes      BYTEA NOT NULL,
    n_bytes    BIGINT NOT NULL,
    created_ms BIGINT NOT NULL,
    n_ops      BIGINT NOT NULL DEFAULT 0,
    from_conn  BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX relay_frames_by_channel ON relay_frames (account_h, stream_id, id);

CREATE TABLE relay_frame_heads (
    frame_id  BIGINT NOT NULL REFERENCES relay_frames (id) ON DELETE CASCADE,
    device_id BYTEA NOT NULL,
    max_seq   BIGINT NOT NULL,
    PRIMARY KEY (frame_id, device_id)
);

CREATE TABLE relay_evicted (
    account_h       BYTEA NOT NULL,
    stream_id       BYTEA NOT NULL,
    device_id       BYTEA NOT NULL,
    evicted_through BIGINT NOT NULL,
    PRIMARY KEY (account_h, stream_id, device_id)
);

CREATE TABLE relay_batches (
    account_h     BYTEA NOT NULL,
    stream_id     BYTEA NOT NULL,
    ops_h         BYTEA NOT NULL,
    frame_id      BIGINT NOT NULL REFERENCES relay_frames (id) ON DELETE CASCADE,
    batch_id      BIGINT NOT NULL,
    first_seen_ms BIGINT NOT NULL,
    PRIMARY KEY (account_h, stream_id, ops_h)
);
CREATE INDEX relay_batches_by_frame ON relay_batches (frame_id);

CREATE TABLE sync_sessions (
    session_h   BYTEA PRIMARY KEY,
    deadline_ms BIGINT,
    seen_ms     BIGINT NOT NULL,
    body        TEXT NOT NULL
);

CREATE TABLE rate_cells (
    key           TEXT PRIMARY KEY,
    tat           NUMERIC NOT NULL,
    idle_at_ms    BIGINT NOT NULL,
    refusing      BOOLEAN NOT NULL,
    first_refusal BOOLEAN NOT NULL
);
CREATE INDEX rate_cells_by_idle ON rate_cells (idle_at_ms);
";

/// 0002: `devices.revoked_by_key`, the counterpart of SQLite's migration 9:
/// the signing key of the device whose signed revoke-by-vault-id request
/// revoked the row (ADR-0056 §5). `NULL` on every row revoked before it.
const DEVICES_REVOKED_BY_KEY: &str = "ALTER TABLE devices ADD COLUMN revoked_by_key TEXT";

/// The table the version lives in. Created by the runner, outside the
/// history, because the runner reads it before any step.
const VERSION_TABLE: &str = r"
CREATE TABLE IF NOT EXISTS schema_version (
    only_row BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (only_row),
    version  INTEGER NOT NULL
)";

/// The database's version: 0 before the first step.
async fn version(client: &impl tokio_postgres::GenericClient) -> Result<i64, OpenError> {
    let row = client
        .query_opt("SELECT version FROM schema_version", &[])
        .await
        .map_err(OpenError::db)?;
    Ok(row.map_or(0, |r| i64::from(r.get::<_, i32>(0))))
}

/// Refuse a version this binary does not know.
fn refuse_newer(found: i64) -> Result<(), OpenError> {
    if found < 0 || found > i64::from(LATEST) {
        return Err(OpenError::SchemaTooNew {
            found,
            supported: LATEST,
        });
    }
    Ok(())
}

/// Bring the database to [`LATEST`], returning the version it started at.
///
/// # Errors
/// [`OpenError::SchemaTooNew`] for a database a newer release migrated, or the
/// statement that failed.
pub(super) async fn migrate(client: &mut Client) -> Result<i64, OpenError> {
    let mut from = None;
    for m in MIGRATIONS {
        let tx = client.transaction().await.map_err(OpenError::db)?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATE_LOCK])
            .await
            .map_err(OpenError::db)?;
        tx.batch_execute(VERSION_TABLE)
            .await
            .map_err(OpenError::db)?;
        let at = version(&tx).await?;
        refuse_newer(at)?;
        from.get_or_insert(at);
        if at >= i64::from(m.id) {
            // Dropping the transaction rolls it back; it wrote nothing.
            continue;
        }
        tx.batch_execute(m.sql).await.map_err(OpenError::db)?;
        tx.execute(
            "INSERT INTO schema_version (only_row, version) VALUES (TRUE, $1)
             ON CONFLICT (only_row) DO UPDATE SET version = EXCLUDED.version",
            &[&m.id],
        )
        .await
        .map_err(OpenError::db)?;
        tx.commit().await.map_err(OpenError::db)?;
        tracing::info!(
            ev = "srv.store.migrated",
            from_v = at,
            to_v = i64::from(m.id),
            "applied relay database migration {:04} {} (postgres)",
            m.id,
            m.name
        );
    }
    Ok(from.unwrap_or(0))
}

/// Set the stored version to `v`, for the test that a newer database is
/// refused.
#[cfg(test)]
pub(super) async fn stamp(client: &Client, v: i32) {
    client
        .execute("UPDATE schema_version SET version = $1", &[&v])
        .await
        .unwrap();
}

/// The SQLite history this baseline matches. A new SQLite migration fails
/// [`tests::a_new_sqlite_migration_has_a_postgres_counterpart`] until this
/// list has its counterpart and this number moves with it: nothing else would
/// notice the Postgres schema falling behind.
#[cfg(test)]
const SQLITE_COVERED: u32 = 9;

#[cfg(test)]
mod tests {
    /// The two histories move together.
    #[test]
    fn a_new_sqlite_migration_has_a_postgres_counterpart() {
        assert_eq!(
            crate::store::migrations::LATEST,
            super::SQLITE_COVERED,
            "a SQLite migration was added: add the same change to the Postgres history in \
             store/postgres/migrations.rs, then move SQLITE_COVERED"
        );
    }
}
