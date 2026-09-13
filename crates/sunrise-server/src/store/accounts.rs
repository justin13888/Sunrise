//! The account row: the pair it is keyed on, and the identity material written
//! onto it once.
//!
//! One table and every statement over it, which is what makes these cohere.
//! `docs/06-server/auth.md` keys an account on the OIDC pair `(iss, sub)`, so
//! this is the only half of the store that sees a [`Subject`]: it provisions on
//! first login behind the `allow_signup` gate, keeps the discovery email in
//! step with the issuer that owns it, and takes the write-once identity keys
//! and recovery blob the first device uploads.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::auth::Subject;

use super::{mint_id, unsigned, Store, StoreError};

/// The `accounts` table.
pub(super) const SCHEMA: &str = r"
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
);";

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

impl Store {
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
    /// Idempotent **for identical input**: a client that retries
    /// `POST /accounts` after a dropped response must not end up with a second
    /// account or a changed identity key, so every column is `COALESCE`d onto
    /// what is already there and re-sending the same values changes nothing.
    ///
    /// A *different* `recovery_blob` is not a no-op but a conflict, and a
    /// missing account row is an error rather than a silent success. Neither
    /// is an oversight: see [`StoreError::RecoveryBlobExists`] for why a
    /// second blob is refused rather than dropped.
    ///
    /// # Errors
    /// [`StoreError::RecoveryBlobExists`] when `recovery_blob` is `Some` and
    /// the account already holds a different one. Nothing is written — the
    /// check runs before the `UPDATE`.
    ///
    /// [`StoreError::NotFound`] when no account carries this `account_id`. The
    /// `UPDATE` matches no row and the read-back that follows finds none.
    ///
    /// [`StoreError::Sqlite`] if either statement fails.
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
            // The blob is write-once, and a second *different* one is a
            // conflict rather than a no-op: see `StoreError::RecoveryBlobExists`
            // for why silently coalescing it was worse than refusing.
            if let Some(offered) = recovery_blob {
                if let Some(existing) = select_recovery_blob(&conn, account_id)? {
                    if existing != offered {
                        return Err(StoreError::RecoveryBlobExists);
                    }
                }
            }
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

    /// The account's sealed recovery blob, as it was uploaded.
    ///
    /// Opaque base64url ciphertext. The server has never introspected it and
    /// does not start here — `docs/06-server/relay-and-blob-storage.md` records
    /// the column as "ciphertext, opaque", and the only thing that can open it
    /// is the user's offline recovery code.
    ///
    /// `Ok(None)` means the account exists and has no blob, which is a `404` to
    /// the caller and not an error here: an account created before the client
    /// ever sealed one is the ordinary state of every account on this relay
    /// today.
    ///
    /// # Errors
    /// SQLite failures.
    pub fn recovery_blob(&self, account_id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.conn.lock();
        Ok(select_recovery_blob(&conn, account_id)?)
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

/// The stored blob for an account, or `None` when the account has none — or
/// does not exist, which the callers already distinguish by other means.
fn select_recovery_blob(
    conn: &Connection,
    account_id: &str,
) -> Result<Option<String>, rusqlite::Error> {
    conn.query_row(
        "SELECT recovery_blob FROM accounts WHERE account_id = ?1",
        params![account_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    .map(Option::flatten)
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
