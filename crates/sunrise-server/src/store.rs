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
    conn: Mutex<Connection>,
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
             nickname, platform, app_version, created_at_ms, last_seen_at_ms, revoked) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, 0)",
            params![
                device_id,
                account_id,
                new.device_pub_s,
                new.device_pub_d,
                new.device_cert,
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
            "SELECT device_id, account_id, device_pub_s, device_pub_d, nickname, platform, \
             app_version, created_at_ms, last_seen_at_ms, revoked, revoked_at_ms \
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
                "SELECT device_id, account_id, device_pub_s, device_pub_d, nickname, platform, \
                 app_version, created_at_ms, last_seen_at_ms, revoked, revoked_at_ms \
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

fn row_to_device(r: &rusqlite::Row<'_>) -> Result<Device, rusqlite::Error> {
    Ok(Device {
        device_id: r.get(0)?,
        account_id: r.get(1)?,
        device_pub_s: r.get(2)?,
        device_pub_d: r.get(3)?,
        nickname: r.get(4)?,
        platform: r.get(5)?,
        app_version: r.get(6)?,
        created_at_ms: unsigned(r.get::<_, i64>(7)?),
        last_seen_at_ms: unsigned(r.get::<_, i64>(8)?),
        revoked: r.get::<_, i64>(9)? != 0,
        revoked_at_ms: r.get::<_, Option<i64>>(10)?.map(unsigned),
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
