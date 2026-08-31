//! Frozen byte-exact test vectors for the v1 crypto suite.
//!
//! Every value here is a **literal**, produced once by running the v1
//! implementation and then pinned. Nothing in this crate calls
//! `sunrise-crypto` — it has no dependencies at all — so a vector can only
//! agree with the implementation if the implementation still produces the
//! same bytes. That is the whole point: if a BLAKE3, CBOR, Ed25519, or
//! XChaCha20-Poly1305 change shifts a single byte, the assertions in
//! `sunrise-crypto/tests/frozen_vectors.rs` fail.
//!
//! The vectors are asserted by the `sunrise-crypto` test suite, which
//! dev-depends on this crate. Keeping them here rather than inline keeps the
//! frozen data reviewable as data, and keeps the dependency arrow pointing
//! away from the implementation.
//!
//! **These values MUST NOT change.** A change is a crypto-suite version bump
//! per `docs/03-crypto/key-rotation.md`, not a test fix. Regenerate only
//! alongside a new suite id.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::doc_markdown)]

/// Decode a lowercase hex literal into a fixed-size byte array at compile
/// time. A length or digit mistake in a frozen vector is a compile error.
const fn hex<const N: usize>(s: &str) -> [u8; N] {
    let b = s.as_bytes();
    assert!(b.len() == N * 2, "hex literal has wrong length for [u8; N]");
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = (nibble(b[2 * i]) << 4) | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => panic!("hex literal must be lowercase [0-9a-f]"),
    }
}

/// One `identity_id_from_pub` vector.
///
/// `identity_id = BLAKE3.derive_key("sunrise.identity_id.v1", ID_S_pub, 16)`
/// per `docs/03-crypto/identity-and-device-keys.md`.
#[derive(Debug, Clone, Copy)]
pub struct IdentityIdVector {
    /// 32-byte Ed25519 identity public key (`ID_S_pub`).
    pub id_s_pub: [u8; 32],
    /// Expected 16-byte identity id.
    pub identity_id: [u8; 16],
}

/// Identity-id derivation vectors: two edge inputs, one high-entropy-looking
/// input, and one real Ed25519 public key ([`DEVICE_SIGNING_PUBLIC`]).
pub const IDENTITY_ID_VECTORS: [IdentityIdVector; 4] = [
    IdentityIdVector {
        id_s_pub: [0x00; 32],
        identity_id: hex("b8355ed7bc4b713084929f9f19e79ae0"),
    },
    IdentityIdVector {
        id_s_pub: [0x07; 32],
        identity_id: hex("9346b7f6a212ee8c4b0853db80dcc4bd"),
    },
    IdentityIdVector {
        id_s_pub: [0xff; 32],
        identity_id: hex("a4b22132fd9c429e88f1b6d1fd982803"),
    },
    IdentityIdVector {
        id_s_pub: DEVICE_SIGNING_PUBLIC,
        identity_id: hex("88976dc38fa242d7c9e3fc58988f560d"),
    },
];

/// One `BLAKE3.derive_key` vector, 32 bytes of output.
#[derive(Debug, Clone, Copy)]
pub struct KdfVector {
    /// Domain-separation context string.
    pub context: &'static str,
    /// Key material fed to the XOF.
    pub key_material: &'static [u8],
    /// Expected first 32 output bytes.
    pub out_32: [u8; 32],
}

/// `BLAKE3.derive_key` vectors covering distinct contexts and an empty input.
pub const KDF_VECTORS: [KdfVector; 3] = [
    KdfVector {
        context: "sunrise.identity_id.v1",
        key_material: b"key material",
        out_32: hex("247b6daa254cc8174c1b48d1697ad291a44ca9f3f912902dc156ce6930a92e15"),
    },
    KdfVector {
        context: "sunrise.test.v1",
        key_material: b"key",
        out_32: hex("be9e3c34435d157172abb4bab0c939377a39914a19393f4f568ac48f3c1197ac"),
    },
    KdfVector {
        context: "sunrise.stream_key.v1",
        key_material: b"",
        out_32: hex("2ad1ddabad95f545528301b9913d266b8262bd8824fb03c70b6f1f9e0b61a2c5"),
    },
];

/// `stream_root_init(&[0x00; 16])`.
pub const STREAM_ROOT_INIT_ZERO: [u8; 32] =
    hex("f40b1d6b076e1213c777608945e6a49bf84fd08f53616e6463b994dc46414c68");

/// Stream id used by the merkle chain below and by the envelope vectors.
pub const STREAM_ID: [u8; 16] = [0x22; 16];

/// `stream_root_init(&STREAM_ID)`.
pub const STREAM_ROOT_0: [u8; 32] =
    hex("eeee782bd75d80542092184249af6b92acdbb29a91b92f8576491679a68bb466");

/// `stream_root_step(&STREAM_ROOT_0, b"a")`.
pub const STREAM_ROOT_1: [u8; 32] =
    hex("5fb332b958a0000f159ee90c1377b54991b404ee0ca2f59163348eaaa0152525");

/// `stream_root_step(&STREAM_ROOT_1, b"b")`.
pub const STREAM_ROOT_2: [u8; 32] =
    hex("53d7f37110ab645251f591091da6320770cfc0b80700e05aa77128fa39ccd145");

/// Ed25519 signing-key seed used by the envelope vectors. Ed25519 signing is
/// deterministic, so a fixed seed pins the signature bytes exactly.
pub const DEVICE_SIGNING_SECRET: [u8; 32] = [0x11; 32];

/// The Ed25519 public key for [`DEVICE_SIGNING_SECRET`].
pub const DEVICE_SIGNING_PUBLIC: [u8; 32] =
    hex("d04ab232742bb4ab3a1368bd4615e4e6d0224ab71a016baf8520a332c9778737");

/// Device id used by the envelope vectors.
pub const DEVICE_ID: [u8; 16] = [0x33; 16];

/// Inner-op bytes fed to `encode_envelope` in both envelope vectors.
pub const ENVELOPE_INNER: &[u8] = b"inner-op-canonical-cbor";

/// `aead_alg = 0` control envelope: plaintext payload, signature only.
///
/// Frozen at `ENVELOPE_FORMAT_V = 3` / `DOC_SCHEMA_V = 4`: field 1 is `3`,
/// field 5 is the HLC array `[physical_ms, logical]`, field 12 is `4`, and the
/// magic prefix reads `5352 02 0003`.
///
/// Field 12 carries the **document** schema, so the two envelope vectors are
/// re-frozen whenever `DOC_SCHEMA_V` moves. That is a doc-schema change, not a
/// crypto change: the KDF, identity-id, stream-root and AEAD vectors carry no
/// version field and never move for that reason.
///
/// `encode_envelope(ENVELOPE_INNER, STREAM_ID, DEVICE_ID, seq = 7,
/// hlc = [1_700_000_000_000, 0], AeadAlgId::None, epoch = 0, nonce = [0; 24],
/// stream_key = None, DEVICE_SIGNING_SECRET)`.
pub mod signed_only_envelope {
    /// `seq` field.
    pub const SEQ: u64 = 7;
    /// `hlc` field's physical component (its logical component is 0).
    pub const HLC_MS: u64 = 1_700_000_000_000;
    /// `epoch` field (MUST be 0 when `aead_alg = 0`).
    pub const EPOCH: u32 = 0;
    /// `nonce` field (unused when `aead_alg = 0`).
    pub const NONCE: [u8; 24] = [0x00; 24];
    /// Expected wire bytes: magic prefix + canonical CBOR + Ed25519 sig.
    pub const ENCODED: [u8; 185] = super::hex(concat!(
        "5352020003ac010302502222222222222222222222222222222203503333",
        "3333333333333333333333333333040705821b0000018bcfe56800000600",
        "070108000958180000000000000000000000000000000000000000000000",
        "000a57696e6e65722d6f702d63616e6f6e6963616c2d63626f720b584091",
        "4bf575f88c9e2e57f909369367577caa4f0af03aa740536ba180bf3bdf94",
        "3a78a58c9b409b85fda3dd5a2242b68df87c401d8f859eed6d0def366fee",
        "5ecc080c04",
    ));
}

/// `aead_alg = 1` envelope: payload sealed with XChaCha20-Poly1305 under a
/// fixed stream key and a fixed nonce, so the ciphertext is reproducible.
///
/// `encode_envelope(ENVELOPE_INNER, STREAM_ID, DEVICE_ID, seq = 9,
/// hlc = [1_700_000_000_001, 0], AeadAlgId::XChaCha20Poly1305, epoch = 3,
/// nonce = [0x55; 24], stream_key = STREAM_KEY, DEVICE_SIGNING_SECRET)`.
pub mod sealed_envelope {
    /// `seq` field.
    pub const SEQ: u64 = 9;
    /// `hlc` field's physical component (its logical component is 0).
    pub const HLC_MS: u64 = 1_700_000_000_001;
    /// `epoch` field.
    pub const EPOCH: u32 = 3;
    /// `nonce` field.
    pub const NONCE: [u8; 24] = [0x55; 24];
    /// Stream key the payload is sealed under.
    pub const STREAM_KEY: [u8; 32] = [0x44; 32];
    /// Expected wire bytes: magic prefix + canonical CBOR + Ed25519 sig.
    pub const ENCODED: [u8; 202] = super::hex(concat!(
        "5352020003ac010302502222222222222222222222222222222203503333",
        "3333333333333333333333333333040905821b0000018bcfe56801000601",
        "070108030958185555555555555555555555555555555555555555555555",
        "550a58276416c4bb3e46b71d10c45af51e2462649e7331f6d5bbb8d59a8d",
        "bcff501f166c7591bc2c21d2880b58402efabc751a4bfa5c00b4dfcbab00",
        "349ad4a5e0d1747565d5c8e0e18af2df9574c1bdc3c1630eac695543ee73",
        "c308da546d35f28d836eca4e4a51fb3a2b5655070c04",
    ));
}

// ---------------------------------------------------------------------------
// Blob chunks
// ---------------------------------------------------------------------------

/// One blob-chunk nonce derivation, frozen.
///
/// `docs/03-crypto/data-encryption-format.md` §Test vectors requires constant
/// vectors for this derivation specifically: the nonce is *not* stored with
/// the chunk, so an implementation that derives it differently produces
/// ciphertext no other implementation can open — and nothing in a round-trip
/// test would notice, because it would be self-consistent.
#[derive(Debug, Clone, Copy)]
pub struct BlobChunkNonceVector {
    /// Per-blob key.
    pub blob_key: [u8; 32],
    /// Chunk index.
    pub chunk_idx: u32,
    /// Expected 24-byte nonce.
    pub nonce: [u8; 24],
}

/// Nonce vectors at the first, second and a far index.
pub const BLOB_CHUNK_NONCE_VECTORS: [BlobChunkNonceVector; 3] = [
    BlobChunkNonceVector {
        blob_key: [0x11; 32],
        chunk_idx: 0,
        nonce: hex("9fc613c31eb65eb42ebd54bd790f2c7d0f9811e90a848111"),
    },
    BlobChunkNonceVector {
        blob_key: [0x11; 32],
        chunk_idx: 1,
        nonce: hex("f4a943628c81b445f7218db847298edb7f0d7e731415139f"),
    },
    BlobChunkNonceVector {
        blob_key: [0x11; 32],
        chunk_idx: 65_535,
        nonce: hex("8f90d729bddf642ad4748ff95b7eca11938ba4d3e8a73421"),
    },
];

/// A frozen sealed chunk, so the AAD and the AEAD binding are pinned too.
pub mod blob_chunk {
    /// Per-blob key.
    pub const BLOB_KEY: [u8; 32] = [0x11; 32];
    /// Blob id.
    pub const BLOB_ID: [u8; 16] = [0x22; 16];
    /// Chunk index.
    pub const CHUNK_IDX: u32 = 1;
    /// Total chunks in the blob.
    pub const CHUNK_COUNT: u32 = 3;
    /// Plaintext of the chunk.
    pub const PLAINTEXT: &[u8] = b"sunrise blob chunk vector";
    /// Canonical CBOR AAD: `{1: blob_id, 2: chunk_idx, 3: chunk_count}`.
    pub const AAD: [u8; 23] = super::hex("a301502222222222222222222222222222222202010303");
    /// Expected `ciphertext || tag`.
    pub const SEALED: [u8; 41] = super::hex(concat!(
        "d20f50e4fc0ee6d009ca35b2fcbf316c3d283653fb22168c93",
        "fe841b1c37e50c6e977586204024de63",
    ));
}
