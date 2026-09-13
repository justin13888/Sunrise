//! The pre-ADR-0024 adoption path: everything a vault written before the key
//! hierarchy needs on its first open under it.
//!
//! These four items cohere because they all exist for one event that happens
//! at most once per vault — [`Keychain::adopt_legacy_vault`](super::Keychain::adopt_legacy_vault)
//! walking a vault whose `local_identity` row predates the `identity` table.
//! The epoch every such vault pinned, the derivation its ops were sealed
//! under, the set of streams it could hold ops for, and the labels to carry
//! over from its self-signed cert: nothing else in the keychain calls any of
//! them, and all four are deleted together at 1.0.

use super::to16;
use sunrise_crypto::blake3_kdf::derive_key_32;
use sunrise_crypto::{DeviceCert, StreamKey, VaultRootKey};
use sunrise_domain::INBOX_STREAM_BYTES;
use sunrise_storage::Db;
use zeroize::Zeroize;

/// The epoch every pre-ADR-0024 op was sealed under.
pub(super) const LEGACY_EPOCH: u32 = 1;

/// The pre-ADR-0024 Stream-key derivation.
///
/// **Do not call this for anything but legacy adoption.** It is the line
/// ADR-0024 deletes: because every device holds the vault root, every device
/// can compute every epoch, so bumping the epoch rotates the ciphertext without
/// rotating the secret. It survives only because a vault written before the
/// change has ops sealed under these exact bytes, and adopting it means
/// recomputing them once.
///
/// Every stream a pre-ADR-0024 vault could hold ops for.
///
/// The two fixed ids are in the set unconditionally: the vault-meta stream
/// never has a `streams` row, and the Inbox's only appears once a task has
/// landed in it.
///
/// Delete at 1.0, along with
/// [`Keychain::adopt_legacy_vault`](super::Keychain::adopt_legacy_vault).
pub(super) fn legacy_stream_set(db: &Db) -> rusqlite::Result<std::collections::BTreeSet<[u8; 16]>> {
    let mut streams: std::collections::BTreeSet<[u8; 16]> = std::collections::BTreeSet::new();
    streams.insert(crate::engine::META_STREAM);
    streams.insert(INBOX_STREAM_BYTES);
    let conn = db.conn();
    for sql in [
        "SELECT DISTINCT stream_id FROM ops",
        "SELECT stream_id FROM streams",
    ] {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
        for row in rows {
            if let Some(id) = to16(&row?) {
                streams.insert(id);
            }
        }
    }
    Ok(streams)
}

/// Delete at 1.0, along with
/// [`Keychain::adopt_legacy_vault`](super::Keychain::adopt_legacy_vault).
pub(super) fn legacy_derived_stream_key(
    vault_root: &VaultRootKey,
    stream_id: &[u8; 16],
    epoch: u32,
) -> StreamKey {
    let mut km = Vec::with_capacity(32 + 16 + 4);
    km.extend_from_slice(vault_root.as_bytes());
    km.extend_from_slice(stream_id);
    km.extend_from_slice(&epoch.to_be_bytes());
    let key = derive_key_32("sunrise.stream_key.v1", &km);
    km.zeroize();
    StreamKey::from_bytes(key)
}

/// Read the nickname/platform out of a legacy self-signed cert so adoption
/// keeps the device's name rather than renaming it behind the user's back.
pub(super) fn legacy_cert_labels(cert_blob: &[u8]) -> (String, String) {
    DeviceCert::from_cbor(cert_blob).map_or_else(
        |_| {
            (
                "sunrise-device".to_string(),
                std::env::consts::OS.to_string(),
            )
        },
        |cert| (cert.body.nickname, cert.body.platform),
    )
}
