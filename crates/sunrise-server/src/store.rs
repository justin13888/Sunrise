//! Self-host account + device persistence (SQLite).
//!
//! `docs/06-server/auth.md` keys an account on the OIDC pair `(iss, sub)` and
//! hangs devices off it; `docs/06-server/api.md` describes the device rows the
//! REST surface returns. This module is the only place either shape is
//! written.
//!
//! Backing file comes from [`crate::ServerConfig::sqlite_path`]; `None` opens
//! an in-memory database, which is what tests use — every `ServerState` then
//! owns a private database and no test can observe another's rows.
//!
//! # Concurrency
//!
//! One `Connection` behind a `parking_lot::Mutex`. Every statement here is a
//! point lookup or a single-row write against a small table, so the critical
//! section is microseconds; a self-host relay does not have the write volume to
//! justify a pool, and a single connection makes "revocation takes effect in
//! the request's transaction" trivially true. A multi-node deployment replaces
//! this module with Postgres.

use std::path::Path;

use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::auth::Subject;

/// Why a store operation failed.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Underlying SQLite failure.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The caller's `(iss, sub)` has no account and `allow_signup` is false.
    #[error("sign-up is disabled on this server")]
    SignupDisabled,
    /// No such row for this account.
    #[error("not found")]
    NotFound,
}

/// A Sunrise account.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Account {
    /// Sunrise-internal id (Crockford base-32 of 16 bytes).
    pub account_id: String,
    /// OIDC `iss` this account authenticates against.
    pub oidc_iss: String,
    /// OIDC `sub` within that issuer.
    pub oidc_sub: String,
    /// Last-seen `email` claim, for sharing/discovery only.
    pub email: Option<String>,
    /// Identity Ed25519 public key (base64url no-pad), set by the first device.
    pub identity_pub_s: Option<String>,
    /// Identity X25519 public key (base64url no-pad).
    pub identity_pub_d: Option<String>,
    /// Subscription tier.
    pub tier: String,
    /// Terms acceptance timestamp, ms since epoch.
    pub terms_at_ms: Option<u64>,
    /// Creation timestamp, ms since epoch.
    pub created_at_ms: u64,
}

/// A device row, matching `DeviceMeta` in `docs/06-server/api.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    /// Crockford base-32 of 16 bytes.
    pub device_id: String,
    /// Owning account.
    #[serde(skip)]
    pub account_id: String,
    /// The id this device answers to *inside the vault*, Crockford base-32 of
    /// 16 bytes, when the device supplied one at registration.
    ///
    /// This is the only name a vault can use for a peer: a `device_revoke` op
    /// carries the vault id and no device knows any peer's relay ULID. Without
    /// it a revocation is inexpressible against this API — see
    /// `docs/06-server/api.md` §Revocation.
    pub vault_device_id: Option<String>,
    /// Device Ed25519 signing key, base64url no-pad. Verifies
    /// `X-Sunrise-Device-Sig`.
    pub device_pub_s: String,
    /// Device X25519 key, base64url no-pad.
    pub device_pub_d: Option<String>,
    /// User-visible name.
    pub nickname: String,
    /// `ios` / `android` / `macos` / `windows` / `linux` / `web`.
    pub platform: String,
    /// Reported app version.
    pub app_version: Option<String>,
    /// Registration time, ms since epoch.
    pub created_at_ms: u64,
    /// Last authenticated request, ms since epoch.
    pub last_seen_at_ms: u64,
    /// Whether the device has been revoked.
    pub revoked: bool,
    /// Revocation time, ms since epoch.
    pub revoked_at_ms: Option<u64>,
}

/// Fields `POST /api/v1/devices` supplies.
#[derive(Debug, Clone)]
pub struct NewDevice {
    /// Ed25519 signing key, base64url no-pad.
    pub device_pub_s: String,
    /// The device's vault-side id, Crockford base-32 of 16 bytes.
    pub vault_device_id: Option<String>,
    /// X25519 key, base64url no-pad.
    pub device_pub_d: Option<String>,
    /// Self-signed device certificate, opaque to the server.
    pub device_cert: Option<String>,
    /// User-visible name.
    pub nickname: String,
    /// Platform tag.
    pub platform: String,
    /// Reported app version.
    pub app_version: Option<String>,
}

/// SQLite-backed account/device store.
pub struct Store {
    /// Shared with [`crate::relay_log`], which puts the durable relay op log
    /// in this same database so one file is the whole server's state and a
    /// `tar` of it is a consistent backup.
    pub(crate) conn: Mutex<Connection>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS accounts (
    account_id     TEXT PRIMARY KEY,
    oidc_iss       TEXT NOT NULL,
    oidc_sub       TEXT NOT NULL,
    email          TEXT,
    identity_pub_s TEXT,
    identity_pub_d TEXT,
    recovery_blob  TEXT,
    tier           TEXT NOT NULL DEFAULT 'free',
    terms_at_ms    INTEGER,
    created_at_ms  INTEGER NOT NULL,
    UNIQUE (oidc_iss, oidc_sub)
);

CREATE TABLE IF NOT EXISTS devices (
    device_id       TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    device_pub_s    TEXT NOT NULL,
    device_pub_d    TEXT,
    device_cert     TEXT,
    vault_device_id TEXT,
    nickname        TEXT NOT NULL,
    platform        TEXT NOT NULL,
    app_version     TEXT,
    created_at_ms   INTEGER NOT NULL,
    last_seen_at_ms INTEGER NOT NULL,
    revoked         INTEGER NOT NULL DEFAULT 0,
    revoked_at_ms   INTEGER
);
CREATE INDEX IF NOT EXISTS devices_by_account ON devices(account_id);

CREATE TABLE IF NOT EXISTS push_tokens (
    device_id     TEXT NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
    platform      TEXT NOT NULL,
    token         TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (device_id, platform)
);

-- Durable relay op log (see `relay_log.rs`). `bytes` is the verbatim wire
-- frame: ciphertext the relay forwards and never opens.
CREATE TABLE IF NOT EXISTS relay_frames (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    account_h  BLOB NOT NULL,
    stream_id  BLOB NOT NULL,
    bytes      BLOB NOT NULL,
    n_bytes    INTEGER NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS relay_frames_by_channel
    ON relay_frames(account_h, stream_id, id);

-- Routing heads, read from each op's cleartext envelope header. This is the
-- only part of a frame the relay ever parses, and it is what makes
-- cursor-filtered replay possible without opening the ciphertext.
CREATE TABLE IF NOT EXISTS relay_frame_heads (
    frame_id  INTEGER NOT NULL REFERENCES relay_frames(id) ON DELETE CASCADE,
    device_id BLOB NOT NULL,
    max_seq   INTEGER NOT NULL,
    PRIMARY KEY (frame_id, device_id)
);

-- Per-channel, per-device high-water mark of what retention has deleted.
-- Survives restart, which is the whole point: an in-memory watermark cannot
-- tell 'never held it' from 'evicted it', so a fresh process reported no gaps
-- and the loss was silent.
CREATE TABLE IF NOT EXISTS relay_evicted (
    account_h       BLOB NOT NULL,
    stream_id       BLOB NOT NULL,
    device_id       BLOB NOT NULL,
    evicted_through INTEGER NOT NULL,
    PRIMARY KEY (account_h, stream_id, device_id)
);

-- One row per op batch the relay has already appended, keyed by the CONTENT of
-- the batch rather than by its `batch_id`. The client's counter is per session
-- (`sync_driver.rs` starts it at 0 inside `session()`), so it restarts at 1 on
-- every reconnect: a `UNIQUE (account_h, device_id, batch_id)` would drop
-- session 2's batch 1 as a duplicate *while acking it*, and an acked batch is
-- deleted from the client's outbox. That is silent data loss. Content is the
-- only key that survives a reconnect, and a reconnect re-draining the outbox
-- is exactly the churn this table exists to absorb.
--
-- `frame_id` is what bounds it: `ON DELETE CASCADE` plus `PRAGMA foreign_keys`
-- means a batch is forgotten the moment retention deletes the frame it named,
-- so the dedup window is the retention window, kept in step for free and with
-- no second sweep to write.
CREATE TABLE IF NOT EXISTS relay_batches (
    account_h     BLOB NOT NULL,
    stream_id     BLOB NOT NULL,
    ops_h         BLOB NOT NULL,
    frame_id      INTEGER NOT NULL REFERENCES relay_frames(id) ON DELETE CASCADE,
    batch_id      INTEGER NOT NULL,
    first_seen_ms INTEGER NOT NULL,
    PRIMARY KEY (account_h, stream_id, ops_h)
);
CREATE INDEX IF NOT EXISTS relay_batches_by_frame ON relay_batches(frame_id);
";

/// Indexes over columns [`add_missing_columns`] may have just added.
///
/// Separate from `SCHEMA` because of the order: `SCHEMA` runs against a
/// database an earlier release created, where the column does not exist yet, so
/// an index naming it there fails with `no such column` and takes the whole
/// startup with it.
const LATE_INDEXES: &str = r"
-- Revocation arrives naming a vault id, so this is the lookup that route runs.
-- Deliberately *not* unique: `docs/06-server/api.md` records that a device
-- re-registering is a second row rather than an error, and all of that device's
-- rows must be revoked together.
CREATE INDEX IF NOT EXISTS devices_by_vault_id
    ON devices(account_id, vault_device_id);
";

impl Store {
    /// Open the store. `None` opens a private in-memory database.
    pub fn open(path: Option<&Path>) -> Result<Self, StoreError> {
        let conn = match path {
            Some(p) => Connection::open(p)?,
            None => Connection::open_in_memory()?,
        };
        // `ON DELETE CASCADE` above is inert unless foreign keys are on, and a
        // revoked device leaving its push tokens behind would keep waking a
        // device its owner believes is gone.
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        add_missing_columns(&conn)?;
        conn.execute_batch(LATE_INDEXES)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Look up the account for a verified subject, provisioning it on first
    /// login when the server allows sign-up.
    ///
    /// This is the `allow_signup` gate from `docs/06-server/auth.md`: with it
    /// false, a token the IdP happily issued still buys nothing here unless an
    /// account already exists.
    pub fn resolve_account(
        &self,
        subject: &Subject,
        allow_signup: bool,
        now_ms: u64,
    ) -> Result<Account, StoreError> {
        let conn = self.conn.lock();
        if let Some(mut account) = select_account_by_oidc(&conn, &subject.issuer, &subject.subject)?
        {
            // Keep the discovery email in step with the IdP, which owns it.
            if let Some(email) = subject.email.as_deref() {
                if account.email.as_deref() != Some(email) {
                    conn.execute(
                        "UPDATE accounts SET email = ?1 WHERE account_id = ?2",
                        params![email, account.account_id],
                    )?;
                    account.email = Some(email.to_string());
                }
            }
            return Ok(account);
        }
        if !allow_signup {
            return Err(StoreError::SignupDisabled);
        }
        let account_id = mint_id(now_ms);
        conn.execute(
            "INSERT INTO accounts (account_id, oidc_iss, oidc_sub, email, tier, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, 'free', ?5)",
            params![
                account_id,
                subject.issuer,
                subject.subject,
                subject.email,
                i64::try_from(now_ms).unwrap_or(i64::MAX),
            ],
        )?;
        Ok(Account {
            account_id,
            oidc_iss: subject.issuer.clone(),
            oidc_sub: subject.subject.clone(),
            email: subject.email.clone(),
            identity_pub_s: None,
            identity_pub_d: None,
            tier: "free".into(),
            terms_at_ms: None,
            created_at_ms: now_ms,
        })
    }

    /// Whether an account already exists for this subject.
    pub fn account_exists(&self, subject: &Subject) -> Result<bool, StoreError> {
        let conn = self.conn.lock();
        Ok(select_account_by_oidc(&conn, &subject.issuer, &subject.subject)?.is_some())
    }

    /// Fetch an account by its Sunrise id.
    pub fn account(&self, account_id: &str) -> Result<Option<Account>, StoreError> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT account_id, oidc_iss, oidc_sub, email, identity_pub_s, identity_pub_d, \
                 tier, terms_at_ms, created_at_ms FROM accounts WHERE account_id = ?1",
                params![account_id],
                row_to_account,
            )
            .optional()?)
    }

    /// Record the identity material the first device registers.
    ///
    /// Idempotent by design: a client that retries `POST /accounts` after a
    /// dropped response must not end up with a second account or a changed
    /// identity key, so the keys are written once and later calls are no-ops.
    pub fn set_identity(
        &self,
        account_id: &str,
        identity_pub_s: &str,
        identity_pub_d: &str,
        recovery_blob: Option<&str>,
        terms_at_ms: u64,
    ) -> Result<Account, StoreError> {
        {
            let conn = self.conn.lock();
            conn.execute(
                "UPDATE accounts
                    SET identity_pub_s = COALESCE(identity_pub_s, ?2),
                        identity_pub_d = COALESCE(identity_pub_d, ?3),
                        recovery_blob  = COALESCE(?4, recovery_blob),
                        terms_at_ms    = COALESCE(terms_at_ms, ?5)
                  WHERE account_id = ?1",
                params![
                    account_id,
                    identity_pub_s,
                    identity_pub_d,
                    recovery_blob,
                    i64::try_from(terms_at_ms).unwrap_or(i64::MAX),
                ],
            )?;
        }
        self.account(account_id)?.ok_or(StoreError::NotFound)
    }

    /// Register a device under an account.
    pub fn register_device(
        &self,
        account_id: &str,
        new: &NewDevice,
        now_ms: u64,
    ) -> Result<Device, StoreError> {
        let device_id = mint_id(now_ms);
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO devices (device_id, account_id, device_pub_s, device_pub_d, device_cert, \
             vault_device_id, nickname, platform, app_version, created_at_ms, last_seen_at_ms, \
             revoked) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, 0)",
            params![
                device_id,
                account_id,
                new.device_pub_s,
                new.device_pub_d,
                new.device_cert,
                new.vault_device_id,
                new.nickname,
                new.platform,
                new.app_version,
                i64::try_from(now_ms).unwrap_or(i64::MAX),
            ],
        )?;
        Ok(Device {
            device_id,
            account_id: account_id.to_string(),
            device_pub_s: new.device_pub_s.clone(),
            device_pub_d: new.device_pub_d.clone(),
            vault_device_id: new.vault_device_id.clone(),
            nickname: new.nickname.clone(),
            platform: new.platform.clone(),
            app_version: new.app_version.clone(),
            created_at_ms: now_ms,
            last_seen_at_ms: now_ms,
            revoked: false,
            revoked_at_ms: None,
        })
    }

    /// Every device on an account, revoked ones included (the API's
    /// `DeviceMeta` carries a `revoked` flag, so the row survives revocation).
    pub fn list_devices(&self, account_id: &str) -> Result<Vec<Device>, StoreError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT device_id, account_id, device_pub_s, device_pub_d, vault_device_id, \
             nickname, platform, app_version, created_at_ms, last_seen_at_ms, revoked, \
             revoked_at_ms \
             FROM devices WHERE account_id = ?1 ORDER BY created_at_ms, device_id",
        )?;
        let rows = stmt.query_map(params![account_id], row_to_device)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Count of devices that can still act on the account.
    pub fn active_device_count(&self, account_id: &str) -> Result<u32, StoreError> {
        let conn = self.conn.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM devices WHERE account_id = ?1 AND revoked = 0",
            params![account_id],
            |r| r.get(0),
        )?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Fetch a device *if it belongs to this account and is not revoked*.
    ///
    /// The account filter is in the SQL rather than checked afterwards: a
    /// lookup by device id alone, compared later, is the shape that leaks
    /// another account's device metadata when the comparison is forgotten.
    pub fn active_device(
        &self,
        account_id: &str,
        device_id: &str,
    ) -> Result<Option<Device>, StoreError> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT device_id, account_id, device_pub_s, device_pub_d, vault_device_id, \
                 nickname, platform, app_version, created_at_ms, last_seen_at_ms, revoked, \
                 revoked_at_ms \
                 FROM devices WHERE account_id = ?1 AND device_id = ?2 AND revoked = 0",
                params![account_id, device_id],
                row_to_device,
            )
            .optional()?)
    }

    /// Revoke a device and drop its push tokens, in one transaction.
    ///
    /// Returns [`StoreError::NotFound`] when the device is not an active device
    /// of this account — a second revoke, or a revoke aimed at someone else's
    /// device, both land here.
    pub fn revoke_device(
        &self,
        account_id: &str,
        device_id: &str,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE devices SET revoked = 1, revoked_at_ms = ?3 \
             WHERE account_id = ?1 AND device_id = ?2 AND revoked = 0",
            params![
                account_id,
                device_id,
                i64::try_from(now_ms).unwrap_or(i64::MAX)
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        tx.execute(
            "DELETE FROM push_tokens WHERE device_id = ?1",
            params![device_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Revoke every active device row carrying `vault_device_id`, and drop
    /// their push tokens, in one transaction.
    ///
    /// This is the route a vault can actually reach. `revoke_device` above
    /// names the relay's own ULID, minted here at registration and never
    /// carried back into the vault; a `device_revoke` op names a 16-byte
    /// vault-side id and no device holds any peer's ULID, so a revocation had
    /// no expressible target until this existed.
    ///
    /// *Every* matching row rather than one, because a device re-registering
    /// with the same keys is a second row rather than an error
    /// (`docs/06-server/api.md`), and every one of those rows is the device the
    /// caller is revoking. Revoking one and leaving the others would leave the
    /// device authenticating under a row the caller has no way to name.
    ///
    /// Returns [`StoreError::NotFound`] when no active row on this account
    /// carries that vault id — a second revoke, a device that never registered
    /// its vault id, and one aimed at another account's device all land here.
    pub fn revoke_devices_by_vault_id(
        &self,
        account_id: &str,
        vault_device_id: &str,
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        // Collected before the update, because afterwards the predicate that
        // selects them no longer holds and the push tokens would survive.
        let ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT device_id FROM devices \
                 WHERE account_id = ?1 AND vault_device_id = ?2 AND revoked = 0",
            )?;
            let rows = stmt.query_map(params![account_id, vault_device_id], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        if ids.is_empty() {
            return Err(StoreError::NotFound);
        }
        tx.execute(
            "UPDATE devices SET revoked = 1, revoked_at_ms = ?3 \
             WHERE account_id = ?1 AND vault_device_id = ?2 AND revoked = 0",
            params![
                account_id,
                vault_device_id,
                i64::try_from(now_ms).unwrap_or(i64::MAX)
            ],
        )?;
        for id in &ids {
            tx.execute("DELETE FROM push_tokens WHERE device_id = ?1", params![id])?;
        }
        tx.commit()?;
        Ok(ids.len())
    }

    /// Record the last time a device made an authenticated request.
    pub fn touch_device(&self, device_id: &str, now_ms: u64) -> Result<(), StoreError> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE devices SET last_seen_at_ms = ?2 WHERE device_id = ?1",
            params![device_id, i64::try_from(now_ms).unwrap_or(i64::MAX)],
        )?;
        Ok(())
    }

    /// Store (or replace) a device's push token for one provider.
    pub fn upsert_push_token(
        &self,
        device_id: &str,
        platform: &str,
        token: &str,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO push_tokens (device_id, platform, token, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(device_id, platform) DO UPDATE SET token = ?3, updated_at_ms = ?4",
            params![
                device_id,
                platform,
                token,
                i64::try_from(now_ms).unwrap_or(i64::MAX)
            ],
        )?;
        Ok(())
    }

    /// Every push token registered for a device, as `(platform, token)`.
    pub fn push_tokens(&self, device_id: &str) -> Result<Vec<(String, String)>, StoreError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT platform, token FROM push_tokens WHERE device_id = ?1 ORDER BY platform",
        )?;
        let rows = stmt.query_map(params![device_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

fn select_account_by_oidc(
    conn: &Connection,
    iss: &str,
    sub: &str,
) -> Result<Option<Account>, rusqlite::Error> {
    conn.query_row(
        "SELECT account_id, oidc_iss, oidc_sub, email, identity_pub_s, identity_pub_d, tier, \
         terms_at_ms, created_at_ms FROM accounts WHERE oidc_iss = ?1 AND oidc_sub = ?2",
        params![iss, sub],
        row_to_account,
    )
    .optional()
}

fn row_to_account(r: &rusqlite::Row<'_>) -> Result<Account, rusqlite::Error> {
    Ok(Account {
        account_id: r.get(0)?,
        oidc_iss: r.get(1)?,
        oidc_sub: r.get(2)?,
        email: r.get(3)?,
        identity_pub_s: r.get(4)?,
        identity_pub_d: r.get(5)?,
        tier: r.get(6)?,
        terms_at_ms: r.get::<_, Option<i64>>(7)?.map(unsigned),
        created_at_ms: unsigned(r.get::<_, i64>(8)?),
    })
}

/// Columns added to a table that already exists on a running relay.
///
/// `SCHEMA` is `CREATE TABLE IF NOT EXISTS`, so it is inert against a database
/// a previous release created and a column added there is never applied. There
/// is no migration framework here and this does not want to become one: SQLite
/// has no `ADD COLUMN IF NOT EXISTS`, so the presence check is `PRAGMA
/// table_info` and the whole mechanism is one idempotent statement per column.
///
/// Every column added this way must be nullable with no default, which is the
/// only shape `ALTER TABLE ... ADD COLUMN` applies to a populated table without
/// rewriting it.
fn add_missing_columns(conn: &Connection) -> Result<(), StoreError> {
    add_column_if_absent(conn, "devices", "vault_device_id", "TEXT")
}

/// One idempotent `ALTER TABLE ... ADD COLUMN`.
///
/// SQLite has no `ADD COLUMN IF NOT EXISTS`, so the presence check is a
/// `pragma_table_info` count. `table`, `column` and `decl` are interpolated
/// into the statement because SQLite binds values and not identifiers; every
/// caller is a literal in this file, and nothing here takes one from a request.
fn add_column_if_absent(
    conn: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> Result<(), StoreError> {
    let present: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
        params![table, column],
        |r| r.get(0),
    )?;
    if present == 0 {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
    }
    Ok(())
}

fn row_to_device(r: &rusqlite::Row<'_>) -> Result<Device, rusqlite::Error> {
    Ok(Device {
        device_id: r.get(0)?,
        account_id: r.get(1)?,
        device_pub_s: r.get(2)?,
        device_pub_d: r.get(3)?,
        vault_device_id: r.get(4)?,
        nickname: r.get(5)?,
        platform: r.get(6)?,
        app_version: r.get(7)?,
        created_at_ms: unsigned(r.get::<_, i64>(8)?),
        last_seen_at_ms: unsigned(r.get::<_, i64>(9)?),
        revoked: r.get::<_, i64>(10)? != 0,
        revoked_at_ms: r.get::<_, Option<i64>>(11)?.map(unsigned),
    })
}

fn unsigned(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// Mint a fresh 16-byte id, rendered Crockford base-32 per
/// `docs/06-server/api.md`.
///
/// ULID layout: the injected clock's millisecond stamp in the high 48 bits,
/// 80 bits of OS entropy below it. Timestamp-prefixed ids keep the SQLite
/// primary-key index appending rather than inserting into the middle, and the
/// entropy is what makes an id unguessable. `getrandom` is used directly rather
/// than a `rand` RNG because the determinism gate bans ambient `rand` sources
/// and there is nothing here worth seeding deterministically — an id a test
/// could predict would be an id an attacker could predict.
#[must_use]
pub fn mint_id(now_ms: u64) -> String {
    let mut random = [0u8; 10];
    if getrandom::getrandom(&mut random).is_err() {
        // No OS entropy is not a condition we can paper over with a
        // predictable id, so fall back to a value that is at least unique
        // per-process-per-ms and let the UNIQUE constraints catch collisions.
        random[..8].copy_from_slice(&now_ms.to_be_bytes());
    }
    let ulid = sunrise_id::ulid::Ulid::from_timestamp_and_random(now_ms, random);
    sunrise_id::crockford::encode_bytes(ulid.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject(sub: &str) -> Subject {
        Subject::new("https://idp.example", sub)
    }

    fn store() -> Store {
        Store::open(None).unwrap()
    }

    const NOW: u64 = 1_704_067_200_000;

    #[test]
    fn a_returning_subject_gets_the_same_account() {
        let s = store();
        let a = s.resolve_account(&subject("alice"), true, NOW).unwrap();
        let b = s
            .resolve_account(&subject("alice"), true, NOW + 5000)
            .unwrap();
        assert_eq!(a.account_id, b.account_id);
        assert_eq!(a.created_at_ms, b.created_at_ms);
    }

    /// The `(iss, sub)` pair is the key, so the same `sub` at a different
    /// issuer is a different person.
    #[test]
    fn the_same_sub_at_another_issuer_is_another_account() {
        let s = store();
        let a = s.resolve_account(&subject("alice"), true, NOW).unwrap();
        let b = s
            .resolve_account(&Subject::new("https://other.example", "alice"), true, NOW)
            .unwrap();
        assert_ne!(a.account_id, b.account_id);
    }

    #[test]
    fn signup_disabled_refuses_an_unknown_subject_but_not_a_known_one() {
        let s = store();
        assert!(matches!(
            s.resolve_account(&subject("alice"), false, NOW),
            Err(StoreError::SignupDisabled)
        ));
        let a = s.resolve_account(&subject("alice"), true, NOW).unwrap();
        // Now that the account exists, the guard no longer applies: it gates
        // provisioning, not login.
        let b = s.resolve_account(&subject("alice"), false, NOW).unwrap();
        assert_eq!(a.account_id, b.account_id);
    }

    #[test]
    fn account_ids_are_not_derived_from_anything_the_caller_supplies() {
        let s = store();
        let a = s.resolve_account(&subject("alice"), true, NOW).unwrap();
        assert_eq!(a.account_id.len(), sunrise_id::crockford::ENCODED_LEN);
        assert!(
            !a.account_id.contains("alice"),
            "the account id must not encode the OIDC subject"
        );
    }

    #[test]
    fn revocation_hides_the_device_from_the_active_lookup_and_drops_push_tokens() {
        let s = store();
        let acct = s.resolve_account(&subject("alice"), true, NOW).unwrap();
        let d = s
            .register_device(
                &acct.account_id,
                &NewDevice {
                    device_pub_s: "k".into(),
                    device_pub_d: None,
                    device_cert: None,
                    vault_device_id: None,
                    nickname: "laptop".into(),
                    platform: "linux".into(),
                    app_version: None,
                },
                NOW,
            )
            .unwrap();
        s.upsert_push_token(&d.device_id, "fcm", "tok", NOW)
            .unwrap();
        assert_eq!(s.push_tokens(&d.device_id).unwrap().len(), 1);
        assert_eq!(s.active_device_count(&acct.account_id).unwrap(), 1);

        s.revoke_device(&acct.account_id, &d.device_id, NOW + 1)
            .unwrap();

        assert!(s
            .active_device(&acct.account_id, &d.device_id)
            .unwrap()
            .is_none());
        assert_eq!(s.active_device_count(&acct.account_id).unwrap(), 0);
        assert!(
            s.push_tokens(&d.device_id).unwrap().is_empty(),
            "a revoked device must stop being wakeable"
        );
        // The row survives so `DeviceMeta.revoked` can be reported.
        let listed = s.list_devices(&acct.account_id).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].revoked);
        assert_eq!(listed[0].revoked_at_ms, Some(NOW + 1));
    }

    #[test]
    fn one_account_cannot_revoke_or_read_anothers_device() {
        let s = store();
        let alice = s.resolve_account(&subject("alice"), true, NOW).unwrap();
        let bob = s.resolve_account(&subject("bob"), true, NOW).unwrap();
        let d = s
            .register_device(
                &alice.account_id,
                &NewDevice {
                    device_pub_s: "k".into(),
                    device_pub_d: None,
                    device_cert: None,
                    vault_device_id: None,
                    nickname: "laptop".into(),
                    platform: "linux".into(),
                    app_version: None,
                },
                NOW,
            )
            .unwrap();
        assert!(matches!(
            s.revoke_device(&bob.account_id, &d.device_id, NOW),
            Err(StoreError::NotFound)
        ));
        assert!(s
            .active_device(&bob.account_id, &d.device_id)
            .unwrap()
            .is_none());
        assert!(s
            .active_device(&alice.account_id, &d.device_id)
            .unwrap()
            .is_some());
    }

    #[test]
    fn revoking_twice_reports_not_found() {
        let s = store();
        let acct = s.resolve_account(&subject("alice"), true, NOW).unwrap();
        let d = s
            .register_device(
                &acct.account_id,
                &NewDevice {
                    device_pub_s: "k".into(),
                    device_pub_d: None,
                    device_cert: None,
                    vault_device_id: None,
                    nickname: "phone".into(),
                    platform: "ios".into(),
                    app_version: None,
                },
                NOW,
            )
            .unwrap();
        s.revoke_device(&acct.account_id, &d.device_id, NOW)
            .unwrap();
        assert!(matches!(
            s.revoke_device(&acct.account_id, &d.device_id, NOW),
            Err(StoreError::NotFound)
        ));
    }

    /// A vault id is a *filter* on the caller's own rows, like every other
    /// device id in this store, and this is the test that says so.
    ///
    /// Two accounts can hold the same vault device id without contriving
    /// anything — nothing coordinates 16-byte ids across accounts — so a
    /// lookup by vault id alone, compared afterwards, would revoke a stranger's
    /// device on a request that had every right to be made.
    #[test]
    fn one_account_cannot_revoke_anothers_device_by_its_vault_id() {
        const VAULT_ID: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
        let s = store();
        let alice = s.resolve_account(&subject("alice"), true, NOW).unwrap();
        let bob = s.resolve_account(&subject("bob"), true, NOW).unwrap();
        let d = s
            .register_device(
                &alice.account_id,
                &NewDevice {
                    device_pub_s: "k".into(),
                    device_pub_d: None,
                    device_cert: None,
                    vault_device_id: Some(VAULT_ID.into()),
                    nickname: "laptop".into(),
                    platform: "linux".into(),
                    app_version: None,
                },
                NOW,
            )
            .unwrap();

        assert!(matches!(
            s.revoke_devices_by_vault_id(&bob.account_id, VAULT_ID, NOW),
            Err(StoreError::NotFound)
        ));
        assert!(
            s.active_device(&alice.account_id, &d.device_id)
                .unwrap()
                .is_some(),
            "alice's device must still be active"
        );

        assert_eq!(
            s.revoke_devices_by_vault_id(&alice.account_id, VAULT_ID, NOW)
                .unwrap(),
            1
        );
        assert!(s
            .active_device(&alice.account_id, &d.device_id)
            .unwrap()
            .is_none());
    }

    /// A column added to a table an earlier release created.
    ///
    /// `SCHEMA` is `CREATE TABLE IF NOT EXISTS`, so it is inert against an
    /// existing database: without `add_missing_columns` a relay upgraded in
    /// place would answer every query touching `vault_device_id` with "no such
    /// column", which is every device query there is.
    #[test]
    fn a_column_added_after_release_reaches_a_database_that_predates_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sunrise.db");
        {
            // The `devices` table as it stood before this column existed.
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE devices (
                     device_id       TEXT PRIMARY KEY,
                     account_id      TEXT NOT NULL,
                     device_pub_s    TEXT NOT NULL,
                     device_pub_d    TEXT,
                     device_cert     TEXT,
                     nickname        TEXT NOT NULL,
                     platform        TEXT NOT NULL,
                     app_version     TEXT,
                     created_at_ms   INTEGER NOT NULL,
                     last_seen_at_ms INTEGER NOT NULL,
                     revoked         INTEGER NOT NULL DEFAULT 0,
                     revoked_at_ms   INTEGER
                 );",
            )
            .unwrap();
        }

        let s = Store::open(Some(&path)).unwrap();
        let acct = s.resolve_account(&subject("alice"), true, NOW).unwrap();
        let d = s
            .register_device(
                &acct.account_id,
                &NewDevice {
                    device_pub_s: "k".into(),
                    device_pub_d: None,
                    device_cert: None,
                    vault_device_id: Some("01J8ZQ7X9K3M5N7P9R1T3V5W7Y".into()),
                    nickname: "laptop".into(),
                    platform: "linux".into(),
                    app_version: None,
                },
                NOW,
            )
            .unwrap();
        assert_eq!(
            s.list_devices(&acct.account_id).unwrap()[0].vault_device_id,
            Some("01J8ZQ7X9K3M5N7P9R1T3V5W7Y".into())
        );
        assert!(s
            .active_device(&acct.account_id, &d.device_id)
            .unwrap()
            .is_some());
    }

    #[test]
    fn identity_material_is_written_once_and_not_overwritten_by_a_retry() {
        let s = store();
        let acct = s.resolve_account(&subject("alice"), true, NOW).unwrap();
        let first = s
            .set_identity(&acct.account_id, "PUB_S", "PUB_D", Some("blob"), NOW)
            .unwrap();
        assert_eq!(first.identity_pub_s.as_deref(), Some("PUB_S"));
        let retry = s
            .set_identity(&acct.account_id, "ATTACKER", "ATTACKER", None, NOW + 1)
            .unwrap();
        assert_eq!(
            retry.identity_pub_s.as_deref(),
            Some("PUB_S"),
            "a second POST /accounts must not swap the identity key"
        );
        assert_eq!(retry.terms_at_ms, Some(NOW));
    }

    #[test]
    fn a_file_backed_store_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sunrise.db");
        let account_id = {
            let s = Store::open(Some(&path)).unwrap();
            s.resolve_account(&subject("alice"), true, NOW)
                .unwrap()
                .account_id
        };
        let s = Store::open(Some(&path)).unwrap();
        assert_eq!(
            s.resolve_account(&subject("alice"), false, NOW)
                .unwrap()
                .account_id,
            account_id,
            "a restart must not orphan existing accounts"
        );
    }

    #[test]
    fn minted_ids_do_not_repeat_within_a_millisecond() {
        let a = mint_id(NOW);
        let b = mint_id(NOW);
        assert_ne!(a, b);
        assert_eq!(a.len(), sunrise_id::crockford::ENCODED_LEN);
    }
}
