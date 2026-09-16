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
            // **A documented fail-open.** A stream id that is not 16 bytes is
            // dropped, so adoption never recomputes that stream's epoch-1
            // derived key and every op in it becomes unopenable — while
            // adoption reports success. Left open because this runs once, on a
            // pre-ADR-0024 vault, and the alternative is refusing to open a
            // vault at all over one malformed row in a legacy table: the
            // streams that do decode are recovered, and the one that does not
            // was not recoverable by any path.
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
///
/// # A documented fail-open, twice over
///
/// A `cert_blob` that does not decode yields placeholder labels rather than an
/// error; and on the `Ok` path **no signature is verified** — `from_cbor`
/// decodes, and nothing calls `verify` or `verify_binding` — so the nickname and
/// platform carried into the freshly issued, identity-signed cert are
/// unauthenticated strings out of a local row.
///
/// Both stay. The blob is this device's own `local_identity.cert_blob`, written
/// by this device under this vault root; anyone who can rewrite it can rewrite
/// the wrapped secrets beside it, so a signature check here defends against an
/// attacker who has already won. And the values are labels: they name the device
/// in a list, they bind nothing, and refusing to adopt a legacy vault because
/// its old cert will not parse would lose the vault over a display string.
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

#[cfg(test)]
mod frozen_domain {
    use super::*;
    use sunrise_crypto_test_vectors::at_rest::{LEGACY_STREAM_KEY_VECTORS, LEGACY_VAULT_ROOT};

    /// `sunrise.stream_key.v1`, anchored to frozen literals.
    ///
    /// `sunrise-crypto-test-vectors`'s `KDF_VECTORS` already freezes
    /// `derive_key` under this context — but it does so against a context
    /// string spelled out *in the vector*, which says nothing about whether
    /// the adoption path still uses that spelling. This goes through
    /// [`legacy_derived_stream_key`] itself, which is the difference between
    /// pinning a string and anchoring a constant.
    ///
    /// What is at stake is a pre-ADR-0024 vault: every op in it is sealed
    /// under a key only this derivation can recompute, and there is no second
    /// copy anywhere. A drift here does not fail an adoption — it adopts the
    /// vault and silently reads none of its history.
    #[test]
    fn the_legacy_derivation_is_byte_exact() {
        let root = VaultRootKey::from_bytes(LEGACY_VAULT_ROOT);
        for v in LEGACY_STREAM_KEY_VECTORS {
            assert_eq!(
                legacy_derived_stream_key(&root, &v.stream_id, v.epoch).as_bytes(),
                &v.key,
                "the sunrise.stream_key.v1 legacy derivation drifted at epoch {}",
                v.epoch
            );
        }
    }
}
