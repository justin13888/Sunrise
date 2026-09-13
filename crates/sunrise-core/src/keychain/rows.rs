//! The keychain's row layer: one function per table write, one struct and one
//! loader per table read.
//!
//! Every `INSERT` and every `SELECT` the keychain issues is here, and nothing
//! here does anything else — the statement text, the column order and the
//! blob-to-value decoding live together, so a schema migration has one file to
//! agree with. The two row structs are the boundary: they carry a row as the
//! rest of the module wants it, already checked for length, and they are the
//! only reason anything outside this file knows a column name.
//!
//! Stream-key wrapping happens here rather than in [`super::crypto`] because
//! the wrap is part of writing the row: a Stream key is wrapped by
//! `sunrise_crypto::wrap_stream_key`, which binds `(stream_id, epoch)` itself
//! and needs no AAD of ours.

use super::{to16, to32, Identity, KeySource, KeychainError, RngAdapter};
use crate::config::Rng;
use rusqlite::{params, OptionalExtension};
use sunrise_crypto::{stream_key_id, wrap_stream_key, StreamKey, VaultRootKey};
use sunrise_storage::Db;

/// Write the account identity row.
///
/// `minted_by` is `Some` only on the two paths that actually generate
/// `ID_S`/`ID_D` — a founding vault and a pre-0017 vault being adopted — and
/// `None` when the identity arrived in a `PairingPayload`. It is what lets
/// [`unwrap_identity`](super::crypto::unwrap_identity) tell "this row holds the
/// only copy of `ID_D_priv`" from "this row holds a copy this device should not
/// have", which migration 0018 could not and 0019 backfills.
pub(super) fn insert_identity_row(
    tx: &rusqlite::Transaction<'_>,
    identity: &Identity,
    wrapped: &(Vec<u8>, Vec<u8>),
    minted_by: Option<&[u8; 16]>,
    now_ms: u64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO identity
         (id, identity_id, id_s_pub, id_d_pub, id_s_priv_wrapped, id_d_priv_wrapped,
          created_at_ms, minted_by_device_id)
         VALUES (1, ?, ?, ?, ?, ?, ?, ?)",
        params![
            &identity.identity_id[..],
            &identity.signing.public_bytes()[..],
            &identity.dh_pub[..],
            wrapped.0,
            wrapped.1,
            now_ms,
            minted_by.map(|d| d.to_vec()),
        ],
    )?;
    Ok(())
}

pub(super) fn insert_local_identity_row(
    tx: &rusqlite::Transaction<'_>,
    device_id: &[u8; 16],
    wrapped_signing: &[u8],
    wrapped_dh: &[u8],
    cert_blob: &[u8],
    now_ms: u64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO local_identity
         (id, device_id, signing_secret_wrapped, dh_secret_wrapped, cert_blob, created_at_ms)
         VALUES (1, ?, ?, ?, ?, ?)",
        params![
            &device_id[..],
            wrapped_signing,
            wrapped_dh,
            cert_blob,
            now_ms
        ],
    )?;
    Ok(())
}

pub(super) fn insert_device_row(
    tx: &rusqlite::Transaction<'_>,
    device_id: &[u8; 16],
    cert_blob: &[u8],
    nickname: &str,
    platform: &str,
    identity_id: &[u8; 16],
    d_d_pub: &[u8; 32],
    now_ms: u64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO devices
         (device_id, cert_blob, nickname, platform, created_at_ms,
          identity_id, d_d_pub)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![
            &device_id[..],
            cert_blob,
            nickname,
            platform,
            now_ms,
            &identity_id[..],
            &d_d_pub[..],
        ],
    )?;
    Ok(())
}

/// Wrap and store one Stream key. Returns `true` if the row was new.
pub(super) fn insert_stream_key_row(
    tx: &rusqlite::Transaction<'_>,
    vault_root: &VaultRootKey,
    stream_id: &[u8; 16],
    epoch: u32,
    key: &StreamKey,
    source: KeySource,
    rng: &dyn Rng,
    now_ms: u64,
) -> rusqlite::Result<bool> {
    let mut adapter = RngAdapter(rng);
    // `wrap_stream_key` can only fail on an AEAD size error, impossible for a
    // 32-byte key.
    let Ok(wrapped) = wrap_stream_key(vault_root, key, stream_id, epoch, &mut adapter) else {
        return Ok(false);
    };
    let key_id = stream_key_id(key);
    tx.execute(
        "INSERT OR IGNORE INTO stream_keys
         (stream_id, epoch, key_id, wrapped, source, created_at_ms)
         VALUES (?, ?, ?, ?, ?, ?)",
        params![
            &stream_id[..],
            epoch,
            &key_id[..],
            wrapped,
            source.as_str(),
            now_ms
        ],
    )?;
    Ok(tx.changes() > 0)
}

/// `(device_id, signing_secret_wrapped, dh_secret_wrapped, cert_blob)`.
pub(super) struct IdentityRow {
    pub(super) device_id: [u8; 16],
    pub(super) signing_wrapped: Vec<u8>,
    pub(super) dh_wrapped: Option<Vec<u8>>,
    pub(super) cert_blob: Vec<u8>,
}

pub(super) struct AccountIdentityRow {
    pub(super) identity_id: [u8; 16],
    pub(super) id_s_pub: [u8; 32],
    pub(super) id_d_pub: [u8; 32],
    pub(super) id_s_priv_wrapped: Vec<u8>,
    pub(super) id_d_priv_wrapped: Vec<u8>,
    /// The device that minted this identity, or `None` when it was adopted
    /// from a `PairingPayload`. Migration 0019 backfills it from
    /// `stream_keys.source`; see that file for why that is a proof and not a
    /// guess.
    pub(super) minted_by_device_id: Option<[u8; 16]>,
    /// When this identity was minted, ms since the epoch.
    pub(super) created_at_ms: u64,
}

pub(super) fn load_identity_row(db: &Db) -> Result<Option<IdentityRow>, KeychainError> {
    let row = db
        .conn()
        .query_row(
            "SELECT device_id, signing_secret_wrapped, dh_secret_wrapped, cert_blob
             FROM local_identity WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(|(d, s, dh, c)| {
        Ok(IdentityRow {
            device_id: to16(&d).ok_or(KeychainError::CorruptRow("local_identity.device_id"))?,
            signing_wrapped: s,
            dh_wrapped: dh,
            cert_blob: c,
        })
    })
    .transpose()
}

pub(super) fn load_account_identity_row(
    db: &Db,
) -> Result<Option<AccountIdentityRow>, KeychainError> {
    let row = db
        .conn()
        .query_row(
            "SELECT identity_id, id_s_pub, id_d_pub, id_s_priv_wrapped, id_d_priv_wrapped,
                    minted_by_device_id, created_at_ms
             FROM identity WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                    r.get::<_, Vec<u8>>(4)?,
                    r.get::<_, Option<Vec<u8>>>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?;
    row.map(|(id, sp, dp, sw, dw, mb, created_at_ms)| {
        Ok(AccountIdentityRow {
            identity_id: to16(&id).ok_or(KeychainError::CorruptRow("identity.identity_id"))?,
            id_s_pub: to32(&sp).ok_or(KeychainError::CorruptRow("identity.id_s_pub"))?,
            id_d_pub: to32(&dp).ok_or(KeychainError::CorruptRow("identity.id_d_pub"))?,
            id_s_priv_wrapped: sw,
            id_d_priv_wrapped: dw,
            // A malformed blob reads as "not this device", which withholds the
            // key rather than granting it. Every other short blob in this
            // loader is a `CorruptRow`, because every other one names something
            // the vault cannot work without.
            minted_by_device_id: mb.as_deref().and_then(to16),
            created_at_ms: u64::try_from(created_at_ms).unwrap_or(0),
        })
    })
    .transpose()
}
