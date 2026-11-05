//! What the relay keeps so a recovery code survives an identity rotation
//! (ADR-0070): the account's identity chain record, and what is left of each
//! recovery blob a rotation replaced.
//!
//! Both are opaque here. The chain record is a list of signed transitions and
//! carry shares HPKE-sealed to keys the relay does not hold; a retired check
//! is the first [`RETIRED_CHECK_LEN`] bytes of a sealed blob, under which the
//! plaintext is a fixed CBOR prefix. The relay can serve, withhold or drop
//! either, which costs a recovery its availability and nothing else.

use rusqlite::{params, OptionalExtension};

use super::{unsigned, Store, StoreError};

/// Migration 0008, and therefore frozen: change these tables by a new
/// migration, never an edit here.
///
/// `identity_chains` is one row per account, replaced conditionally on the
/// digest of the row in place, as a key bundle pin is. `retired_recovery_blobs`
/// keeps a few bytes of each blob a replacement retired, newest
/// [`MAX_RETIRED`] per account.
pub(super) const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS identity_chains (
    account_id      TEXT PRIMARY KEY REFERENCES accounts(account_id) ON DELETE CASCADE,
    chain           BLOB NOT NULL,
    digest          BLOB NOT NULL,
    published_by    TEXT,
    published_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS retired_recovery_blobs (
    account_id    TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    retired_at_ms INTEGER NOT NULL,
    check_prefix  BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS retired_recovery_blobs_by_account
    ON retired_recovery_blobs(account_id, retired_at_ms);";

/// How many bytes of a replaced blob are kept: the five-byte magic, the
/// sixteen-byte salt, the twenty-four-byte nonce and four bytes of
/// ciphertext. `sunrise_crypto::RETIRED_CHECK_LEN` is the same number, and
/// `sunrise-e2e`'s recovery-across-rotation test pins the two equal.
pub const RETIRED_CHECK_LEN: usize = 49;

/// How many retired checks an account keeps. A code is retired by a rotation
/// the user asked for, so a handful covers every code anyone still holds.
pub(crate) const MAX_RETIRED: i64 = 8;

/// The largest identity chain record the relay stores.
pub const MAX_CHAIN_BYTES: usize = 2 * 1024 * 1024;

/// An account's pinned identity chain record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedIdentityChain {
    /// The record, verbatim.
    pub chain: Vec<u8>,
    /// BLAKE3 of `chain`, which a replacement names.
    pub digest: [u8; 32],
    /// The relay device row that pinned it. `None` where no device signed.
    pub published_by: Option<String>,
    /// When it was pinned, ms since the epoch.
    pub published_at_ms: u64,
}

/// What [`Store::pin_identity_chain`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityChainPinned {
    /// The record is in place.
    Pinned,
    /// The record named as replaced is not the one in place. Nothing was
    /// written.
    Superseded,
}

/// What is left of one replaced recovery blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredRecoveryBlob {
    /// When it was replaced, ms since the epoch.
    pub retired_at_ms: u64,
    /// Its first [`RETIRED_CHECK_LEN`] bytes.
    pub check: Vec<u8>,
}

fn signed(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

impl Store {
    /// Make `pin` the account's identity chain if the record in place is the
    /// one whose digest is `replaces` (`None`: no record in place).
    ///
    /// # Errors
    /// SQLite failures.
    pub fn pin_identity_chain(
        &self,
        account_id: &str,
        pin: &PinnedIdentityChain,
        replaces: Option<[u8; 32]>,
    ) -> Result<IdentityChainPinned, StoreError> {
        let mut conn = self.tx("store.pin_identity_chain");
        let tx = conn.transaction()?;
        let before: Option<Vec<u8>> = tx
            .query_row(
                "SELECT digest FROM identity_chains WHERE account_id = ?1",
                params![account_id],
                |r| r.get(0),
            )
            .optional()?;
        let before: Option<[u8; 32]> = before.as_deref().and_then(|b| b.try_into().ok());
        if before != replaces {
            return Ok(IdentityChainPinned::Superseded);
        }
        tx.execute(
            "INSERT INTO identity_chains
                 (account_id, chain, digest, published_by, published_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (account_id) DO UPDATE SET
                 chain = excluded.chain, digest = excluded.digest,
                 published_by = excluded.published_by,
                 published_at_ms = excluded.published_at_ms",
            params![
                account_id,
                pin.chain,
                &pin.digest[..],
                pin.published_by,
                signed(pin.published_at_ms),
            ],
        )?;
        tx.commit()?;
        Ok(IdentityChainPinned::Pinned)
    }

    /// The account's pinned identity chain record, if there is one.
    ///
    /// # Errors
    /// SQLite failures.
    pub fn pinned_identity_chain(
        &self,
        account_id: &str,
    ) -> Result<Option<PinnedIdentityChain>, StoreError> {
        let row = self
            .tx("store.pinned_identity_chain")
            .query_row(
                "SELECT chain, digest, published_by, published_at_ms
                 FROM identity_chains WHERE account_id = ?1",
                params![account_id],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.and_then(|(chain, digest, by, at)| {
            Some(PinnedIdentityChain {
                chain,
                digest: digest.as_slice().try_into().ok()?,
                published_by: by,
                published_at_ms: unsigned(at),
            })
        }))
    }

    /// Replace the account's recovery blob with `blob`, keeping `check` — the
    /// first [`RETIRED_CHECK_LEN`] bytes of the one it replaces — as a
    /// retired check, in one transaction.
    ///
    /// The blob column stops being write-once here, and only here: a device
    /// that rotated the identity without carrying the code re-seals the blob
    /// under a new code (ADR-0070 §2). `POST /accounts` still refuses a second,
    /// differing blob.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when no account carries `account_id`; SQLite
    /// failures.
    pub fn replace_recovery_blob(
        &self,
        account_id: &str,
        blob: &str,
        check: Option<&[u8]>,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let mut conn = self.tx("store.replace_recovery_blob");
        let tx = conn.transaction()?;
        let n = tx.execute(
            "UPDATE accounts SET recovery_blob = ?2 WHERE account_id = ?1",
            params![account_id, blob],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        if let Some(check) = check {
            tx.execute(
                "INSERT INTO retired_recovery_blobs (account_id, retired_at_ms, check_prefix)
                 VALUES (?1, ?2, ?3)",
                params![account_id, signed(now_ms), check],
            )?;
            tx.execute(
                "DELETE FROM retired_recovery_blobs
                 WHERE account_id = ?1 AND rowid NOT IN (
                     SELECT rowid FROM retired_recovery_blobs WHERE account_id = ?1
                     ORDER BY retired_at_ms DESC, rowid DESC LIMIT ?2)",
                params![account_id, MAX_RETIRED],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The account's retired recovery blob checks, newest first.
    ///
    /// # Errors
    /// SQLite failures.
    pub fn retired_recovery_blobs(
        &self,
        account_id: &str,
    ) -> Result<Vec<RetiredRecoveryBlob>, StoreError> {
        let conn = self.tx("store.retired_recovery_blobs");
        let mut stmt = conn.prepare(
            "SELECT retired_at_ms, check_prefix FROM retired_recovery_blobs
             WHERE account_id = ?1 ORDER BY retired_at_ms DESC, rowid DESC",
        )?;
        let rows = stmt
            .query_map(params![account_id], |r| {
                Ok(RetiredRecoveryBlob {
                    retired_at_ms: unsigned(r.get(0)?),
                    check: r.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
