//! The device rows, and the push tokens that cascade off them.
//!
//! `docs/06-server/api.md` describes what these rows return, and every
//! statement that registers, lists, touches or revokes one is here. Both lookup
//! shapes sit together because revocation needs both: the relay's own
//! `device_id`, minted at registration, and the `vault_device_id` that is the
//! only name a peer inside a vault can use.
//!
//! Push tokens are not a third concern. They are `ON DELETE CASCADE` children
//! of a device row, and they are deleted inside the same transaction that
//! revokes one, so the only code that writes them is code already holding the
//! device it is writing for.
//!
//! The in-place column addition is here for the same reason: the only column
//! ever added to a released database is a device column, and
//! [`add_missing_columns`] has to run between [`SCHEMA`] and [`LATE_INDEXES`]
//! for the ordering reason `LATE_INDEXES` records.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{mint_id, unsigned, Store, StoreError};

/// The `devices` table, its by-account index, and the `push_tokens` rows that
/// cascade off it.
pub(super) const SCHEMA: &str = r"
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
);";

/// Indexes over columns [`add_missing_columns`] may have just added.
///
/// Separate from `SCHEMA` because of the order: `SCHEMA` runs against a
/// database an earlier release created, where the column does not exist yet, so
/// an index naming it there fails with `no such column` and takes the whole
/// startup with it.
pub(super) const LATE_INDEXES: &str = r"
-- Revocation arrives naming a vault id, so this is the lookup that route runs.
-- Deliberately *not* unique: `docs/06-server/api.md` records that a device
-- re-registering is a second row rather than an error, and all of that device's
-- rows must be revoked together.
CREATE INDEX IF NOT EXISTS devices_by_vault_id
    ON devices(account_id, vault_device_id);
";

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

impl Store {
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
pub(super) fn add_missing_columns(conn: &Connection) -> Result<(), StoreError> {
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
