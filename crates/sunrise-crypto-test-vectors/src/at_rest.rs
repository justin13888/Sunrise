//! What a vault holds on disk, frozen: the wrapped-secret AADs, the wrapped
//! stream key, the recovery blob, the vault-meta genesis key, the pre-ADR-0024
//! derived stream key, and the op-log's derived primary key.
//!
//! These are the constants a *second build of Sunrise* has to reproduce before
//! it can open a vault this one wrote. None of them travels: an AAD is not
//! stored beside the ciphertext it authenticates, a KDF context is not stored
//! beside its output, and an op-log row's id is recomputed rather than read.
//! So a build that constructs any of them differently writes a vault that
//! looks perfectly healthy to itself and opens nowhere else — which is the
//! failure a round-trip test cannot see and a literal can.
//!
//! Specified by `docs/03-crypto/key-rotation.md`, `docs/03-crypto/recovery.md`,
//! `docs/04-storage/local-database.md` and `docs/04-storage/op-log.md`.

/// A wrapped Stream key, frozen, plus the length the unwrapper checks.
///
/// `wrapped = nonce(24) || XChaCha20-Poly1305_seal(vault_root, nonce,
/// stream_key, "sunrise.wrap.stream_key.v1" || stream_id || u32_be(epoch))`
/// per `docs/03-crypto/key-rotation.md`.
///
/// The AAD is the whole point of the vector. It is not stored, so a build that
/// spelled the prefix differently, or bound the epoch little-endian, would
/// still unwrap everything it wrapped itself. [`WRAPPED`](wrapped_stream_key::WRAPPED)
/// was produced by this build and opens only under this build's AAD.
pub mod wrapped_stream_key {
    /// The vault root the key is wrapped under.
    pub const VAULT_ROOT: [u8; 32] = [0x7a; 32];
    /// The stream the key belongs to.
    pub const STREAM_ID: [u8; 16] = [0x22; 16];
    /// The epoch the key belongs to — bound in the AAD, not in the blob.
    pub const EPOCH: u32 = 3;
    /// The 32-byte Stream key inside the wrap.
    pub const STREAM_KEY: [u8; 32] = [0x77; 32];
    /// `nonce(24) || ciphertext(32) || tag(16)`.
    pub const WRAPPED: [u8; 72] = crate::hex(concat!(
        "061a5101de224ea7fb115222ab2690c62a43259d70d13931ef1aaf3d5f9c",
        "1046a661ffd0933d52eb7e68f045c9febf4f70d2b9f747ce571d7310caee",
        "45402a889005a749ef31ad53",
    ));
    /// `sunrise_crypto::stream_key::WRAPPED_STREAM_KEY_LEN`, as a literal.
    ///
    /// Written down rather than imported so that a coordinated edit to the
    /// constant and to the code that reads it cannot pass: the length is a
    /// wire fact about every `stream_keys.wrapped` column ever written.
    pub const WRAPPED_STREAM_KEY_LEN: usize = 72;
}

/// A whole recovery blob, frozen.
///
/// `magic(5) || salt(16) || nonce(24) || XChaCha20-Poly1305_seal(recovery_key,
/// nonce, canonical_cbor(payload), "sunrise.recovery_blob.v1" || identity_id)`
/// per `docs/03-crypto/recovery.md`.
///
/// One literal pins five things at once: the magic prefix and its version, the
/// Argon2id parameters (`v = 0x13`, `m = 65536 KiB`, `t = 3`, `p = 1`), the
/// salt-then-nonce header layout, the AAD prefix, and the payload's CBOR field
/// numbering. A recovery blob is the last copy of an account that exists, so a
/// build that gets any of those wrong loses the account rather than failing a
/// test — and every one of them is invisible to a seal-then-open test.
pub mod recovery_blob {
    /// The 32-byte BIP-39 derived seed that opens [`BLOB`].
    pub const SEED: [u8; 32] = [0x99; 32];
    /// `identity_id`, bound into the AAD and carried in field 5.
    pub const IDENTITY_ID: [u8; 16] = crate::identity_transition::IDENTITY_ID;
    /// Field 1 of the payload: `ID_S_priv`.
    pub const ID_S_PRIV: [u8; 32] = crate::identity_transition::IDENTITY_SIGNING_SECRET;
    /// Field 2: `ID_D_priv`.
    pub const ID_D_PRIV: [u8; 32] = crate::identity_transition::IDENTITY_DH_SECRET;
    /// Field 3: `ID_S_pub`.
    pub const ID_S_PUB: [u8; 32] = crate::identity_transition::IDENTITY_SIGNING_PUBLIC;
    /// Field 4: `ID_D_pub`.
    pub const ID_D_PUB: [u8; 32] = crate::identity_transition::IDENTITY_DH_PUBLIC;
    /// Field 6: `created_at`, ms since the epoch.
    pub const CREATED_AT_MS: u64 = 1_700_000_000_000;
    /// The sealed blob, as it would sit on the server.
    pub const BLOB: [u8; 230] = crate::hex(concat!(
        "53520300019a4695b87e1883cfa1f55cfdf8226044f36206b50f26427d21",
        "174ed97aa839caa93a5fdf13cc8c1fb0f07ea23b1ba925134799ac7f5e18",
        "6ec11a65a0e90eaae9b2030ec714838fbf05b96f22cfeb44919a550f982b",
        "9c28fb47fe203f6105ff68dcc92f6a1ad80f5a1ec4448290940fddce79a0",
        "c5d81685d07b486600c741e617efbe69b2b56d019d624bf7556863740934",
        "b76b2153bb5cb1d07b5353208b9a46b1cfeb0aa81c23991fcf72d61cb82f",
        "e6207fd6c24df94c177e3c22a50025e02373964b38324b37270205f52375",
        "7e8502477c601c0f99104848e23445b1222606e8",
    ));
}

/// The keychain's four wrapped-secret AADs, frozen whole.
///
/// Each is `prefix || id`, and the prefix is what binds a blob to the *kind*
/// of secret it holds: a device's signing seed must not open as its DH seed,
/// and the account identity's signing half must not open as its DH half. Both
/// pairs were once one domain, and both splits are what `CRYPTO_SUITE_V = 4`
/// names.
///
/// Frozen as the full AAD rather than as the prefix alone so the concatenation
/// order is pinned too. Nothing stores an AAD, so the only way a build learns
/// it built one wrongly is that somebody else's vault will not open.
pub mod keychain_aad {
    /// The device id the two device AADs bind.
    pub const DEVICE_ID: [u8; 16] = crate::identity_transition::device_cert::DEVICE_ID;
    /// The identity id the two identity AADs bind.
    pub const IDENTITY_ID: [u8; 16] = crate::identity_transition::IDENTITY_ID;
    /// `"sunrise.local_identity.v1" || device_id`.
    pub const DEVICE_SIGNING: [u8; 41] = crate::hex(concat!(
        "73756e726973652e6c6f63616c5f6964656e746974792e7631bd7a2df3f4",
        "5482e111edeee969e0168e",
    ));
    /// `"sunrise.local_identity.dh.v1" || device_id`.
    pub const DEVICE_DH: [u8; 44] = crate::hex(concat!(
        "73756e726973652e6c6f63616c5f6964656e746974792e64682e7631bd7a",
        "2df3f45482e111edeee969e0168e",
    ));
    /// `"sunrise.local_identity.identity.sign.v2" || identity_id`.
    pub const IDENTITY_SIGNING: [u8; 55] = crate::hex(concat!(
        "73756e726973652e6c6f63616c5f6964656e746974792e6964656e746974",
        "792e7369676e2e763245a0390256fa6254564d065f60299781",
    ));
    /// `"sunrise.local_identity.identity.dh.v2" || identity_id`.
    pub const IDENTITY_DH: [u8; 53] = crate::hex(concat!(
        "73756e726973652e6c6f63616c5f6964656e746974792e6964656e746974",
        "792e64682e763245a0390256fa6254564d065f60299781",
    ));
    /// `sunrise_core::keychain::crypto::WRAPPED_SECRET_LEN`, as a literal:
    /// nonce (24) + ciphertext (32) + tag (16).
    ///
    /// Written down rather than imported, for the reason
    /// [`super::wrapped_stream_key::WRAPPED_STREAM_KEY_LEN`] is: every
    /// `local_identity` and `identity` blob ever written is exactly this long,
    /// and an edit that changed the constant and its reader together would
    /// otherwise pass.
    pub const WRAPPED_SECRET_LEN: usize = 72;
}

/// The vault-meta stream's genesis key, frozen.
///
/// `BLAKE3.derive_key("sunrise.meta_genesis_key.v1", ID_D_priv || identity_id)`.
///
/// The one Stream key in the hierarchy that is derived rather than drawn, and
/// the one that has to be: every `key_envelope` op lives in the vault-meta
/// stream, so a device restored from the recovery code has to compute this
/// stream's first epoch before it can be *given* any other key. Two builds
/// that derive it differently produce a recovery that reads nothing while
/// looking healthy.
pub mod meta_genesis {
    /// The account's `ID_D_priv`.
    pub const ID_D_PRIV: [u8; 32] = crate::identity_transition::IDENTITY_DH_SECRET;
    /// The `ID_S_priv` that goes with it, because [`IDENTITY_ID`] is derived
    /// from its public half and the derivation takes both.
    pub const ID_S_PRIV: [u8; 32] = crate::identity_transition::IDENTITY_SIGNING_SECRET;
    /// The account's `identity_id`.
    pub const IDENTITY_ID: [u8; 16] = crate::identity_transition::IDENTITY_ID;
    /// The vault-meta stream id: sixteen zero bytes.
    pub const META_STREAM: [u8; 16] = [0u8; 16];
    /// The genesis epoch of any stream.
    pub const GENESIS_EPOCH: u32 = 1;
    /// The derived key.
    pub const KEY: [u8; 32] = crate::hex(concat!(
        "7d125682b92e2cd6cc1198d0dbdb06fd8c7414384c4b51a5b000ba1863d1",
        "407f",
    ));
}

/// One pre-ADR-0024 derived Stream key.
///
/// `BLAKE3.derive_key("sunrise.stream_key.v1", vault_root || stream_id ||
/// u32_be(epoch))`, which is what `legacy_derived_stream_key` recomputes when
/// a vault written before ADR-0024 is adopted.
///
/// [`crate::KDF_VECTORS`] already freezes `derive_key` under this context —
/// but it freezes it against a context string spelled out *in the vector*, so
/// it says nothing about whether the adoption path still uses that spelling.
/// This vector goes through the production function, which is the difference
/// between pinning a string and anchoring a constant.
#[derive(Debug, Clone, Copy)]
pub struct LegacyStreamKeyVector {
    /// Stream the key belongs to.
    pub stream_id: [u8; 16],
    /// Epoch the key belongs to.
    pub epoch: u32,
    /// The derived 32-byte key.
    pub key: [u8; 32],
}

/// The vault root every [`LEGACY_STREAM_KEY_VECTORS`] entry derives under.
pub const LEGACY_VAULT_ROOT: [u8; 32] = [0x7a; 32];

/// Two legacy derivations: the epoch a legacy vault actually used, and a
/// second `(stream, epoch)` so the vector shows both coordinates moving.
pub const LEGACY_STREAM_KEY_VECTORS: [LegacyStreamKeyVector; 2] = [
    LegacyStreamKeyVector {
        stream_id: [0x22; 16],
        epoch: 1,
        key: crate::hex("2d1f98fbc23d57b72efeb34489dc90c51ea0e6ef76bd05a77f7d12346455bbec"),
    },
    LegacyStreamKeyVector {
        stream_id: [0x88; 16],
        epoch: 9,
        key: crate::hex("11bded396a8a58d3fd17079540e8d3db928b272b199ad025b725267a9558bccb"),
    },
];

/// One `remote_op_id` derivation.
///
/// `BLAKE3.derive_key("sunrise.remote_op_id.v1", stream_id || device_id ||
/// u64_be(seq))[..16]`, per `docs/04-storage/op-log.md`.
///
/// This id is a primary key two replicas have to agree on without exchanging
/// it — it is what makes `UNIQUE(stream, device, seq)` idempotent across
/// devices rather than only within one. A build that derived it differently
/// would store every peer's op twice and notice nothing.
#[derive(Debug, Clone, Copy)]
pub struct RemoteOpIdVector {
    /// Stream the op belongs to.
    pub stream_id: [u8; 16],
    /// Device that emitted it.
    pub device_id: [u8; 16],
    /// Per-device sequence number.
    pub seq: u64,
    /// The 16-byte op-log primary key.
    pub op_id: [u8; 16],
}

/// Three `remote_op_id` vectors: both edges of `seq` and one ordinary triple.
pub const REMOTE_OP_ID_VECTORS: [RemoteOpIdVector; 3] = [
    RemoteOpIdVector {
        stream_id: [0x00; 16],
        device_id: [0x00; 16],
        seq: 0,
        op_id: crate::hex("92310f27fdb6fd6f091975b40fb193ef"),
    },
    RemoteOpIdVector {
        stream_id: [0x22; 16],
        device_id: [0x33; 16],
        seq: 7,
        op_id: crate::hex("f4606b7e0305db13d46c9efcf3c6b28a"),
    },
    RemoteOpIdVector {
        stream_id: [0xff; 16],
        device_id: [0xee; 16],
        seq: u64::MAX,
        op_id: crate::hex("d6098eda8e9a2dd39541c360ed337ba9"),
    },
];
