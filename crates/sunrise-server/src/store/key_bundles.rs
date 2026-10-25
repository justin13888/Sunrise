//! The one key bundle the relay keeps per account (ADR-0069).
//!
//! A key bundle is every current Stream key, sealed to the account identity
//! and signed by the device that published it. It is what lets a device
//! restored from the recovery code read anything once the relay has trimmed
//! the `key_envelope` ops that used to be its only source of keys.
//!
//! The table holds a pointer, exactly as `stream_snapshots` does (ADR-0068):
//! the bytes live in the blob backend. Unlike a snapshot pin, a replacement is
//! conditional. A publisher names the pin it read and the relay replaces only
//! that one, so of two honest devices racing, the one that read a stale pin
//! is told so and does not overwrite the other's.

use rusqlite::{params, OptionalExtension};

use super::{unsigned, Store, StoreError};

/// Migration 0007, and therefore frozen: change this table by a new
/// migration, never an edit here.
///
/// One row per account, cascading from the account row. `manifest_digest` is
/// the client's digest of the bundle's cleartext manifest; the relay cannot
/// check it and only hands it back, so a device can tell an unchanged bundle
/// without fetching it.
pub(super) const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS key_bundles (
    account_id      TEXT PRIMARY KEY REFERENCES accounts(account_id) ON DELETE CASCADE,
    blob_key        BLOB NOT NULL,
    size_bytes      INTEGER NOT NULL,
    manifest_digest BLOB NOT NULL,
    published_by    TEXT,
    published_at_ms INTEGER NOT NULL
);";

/// An account's pinned key bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedKeyBundle {
    /// The committed blob holding the bundle, by its content key.
    pub blob_key: [u8; 16],
    /// The bundle's size in bytes, as the blob's manifest states it.
    pub size_bytes: u64,
    /// The publisher's digest of the bundle's manifest, opaque to the relay.
    pub manifest_digest: [u8; 32],
    /// The relay device row that pinned it. `None` where no device signed.
    pub published_by: Option<String>,
    /// When it was pinned, ms since the epoch.
    pub published_at_ms: u64,
}

/// What [`Store::pin_key_bundle`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyBundlePinned {
    /// The pin is in place. `released` is the blob the replaced pin named, for
    /// the caller to delete, when it differs from the new one.
    Pinned {
        /// The replaced pin's blob, if any and if different.
        released: Option<[u8; 16]>,
    },
    /// The pin the caller named is not the one in place: another device
    /// pinned first. Nothing was written.
    Superseded,
}

fn signed(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

impl Store {
    /// Make `pin` the account's key bundle if the bundle in place is the one
    /// named by `replaces` (`None`: no bundle in place).
    ///
    /// The comparison and the write are one transaction, so two publishers
    /// that read the same pin cannot both replace it.
    pub fn pin_key_bundle(
        &self,
        account_id: &str,
        pin: &PinnedKeyBundle,
        replaces: Option<[u8; 16]>,
    ) -> Result<KeyBundlePinned, StoreError> {
        let mut conn = self.tx("store.pin_key_bundle");
        let tx = conn.transaction()?;
        let before: Option<Vec<u8>> = tx
            .query_row(
                "SELECT blob_key FROM key_bundles WHERE account_id = ?1",
                params![account_id],
                |r| r.get(0),
            )
            .optional()?;
        let before: Option<[u8; 16]> = before.as_deref().and_then(|b| b.try_into().ok());
        if before != replaces {
            return Ok(KeyBundlePinned::Superseded);
        }
        tx.execute(
            "INSERT INTO key_bundles
                 (account_id, blob_key, size_bytes, manifest_digest, published_by, published_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (account_id) DO UPDATE SET
                 blob_key = excluded.blob_key, size_bytes = excluded.size_bytes,
                 manifest_digest = excluded.manifest_digest,
                 published_by = excluded.published_by,
                 published_at_ms = excluded.published_at_ms",
            params![
                account_id,
                &pin.blob_key[..],
                signed(pin.size_bytes),
                &pin.manifest_digest[..],
                pin.published_by,
                signed(pin.published_at_ms),
            ],
        )?;
        tx.commit()?;
        Ok(KeyBundlePinned::Pinned {
            released: before.filter(|old| *old != pin.blob_key),
        })
    }

    /// The account's pinned key bundle, if there is one.
    pub fn pinned_key_bundle(
        &self,
        account_id: &str,
    ) -> Result<Option<PinnedKeyBundle>, StoreError> {
        let row = self
            .tx("store.pinned_key_bundle")
            .query_row(
                "SELECT blob_key, size_bytes, manifest_digest, published_by, published_at_ms
                 FROM key_bundles WHERE account_id = ?1",
                params![account_id],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.and_then(|(blob, size, digest, by, at)| {
            Some(PinnedKeyBundle {
                blob_key: blob.as_slice().try_into().ok()?,
                size_bytes: unsigned(size),
                manifest_digest: digest.as_slice().try_into().ok()?,
                published_by: by,
                published_at_ms: unsigned(at),
            })
        }))
    }
}
