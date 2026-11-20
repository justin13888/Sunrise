//! Keying a connection, after the gate that waits for SQLCipher to be able to.

use super::{Db, DbError};
use rusqlite::Connection;
use sunrise_crypto::keys::VaultRootKey;

impl Db {
    pub(crate) fn apply_sqlcipher_key(
        conn: &Connection,
        vault_root: &VaultRootKey,
    ) -> Result<(), DbError> {
        // The once-per-process wait for SQLCipher to finish initialising
        // (issue #521), shared with the relay's store.
        sunrise_sqlcipher::ensure_cipher_ready()
            .map_err(|e| DbError::CipherUnavailable { detail: e.detail })?;
        // Derive a 32-byte SQLCipher raw key from vault_root.
        let raw = sunrise_crypto::derive_key("sunrise.sqlcipher_key.v1", vault_root.as_bytes(), 32);
        let mut hex_buf = String::with_capacity(64);
        for b in &raw {
            use core::fmt::Write;
            let _ = write!(hex_buf, "{b:02x}");
        }
        // SQLCipher's `PRAGMA key = "x'<64 hex>'"` accepts a raw key.
        let key_pragma = format!("PRAGMA key = \"x'{hex_buf}'\"");
        conn.execute_batch(&key_pragma)?;
        // Pre-derived: skip SQLCipher's own KDF iterations.
        conn.execute_batch("PRAGMA kdf_iter = 1;")?;
        Ok(())
    }
}
