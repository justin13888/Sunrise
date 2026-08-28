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
/// Frozen at `ENVELOPE_FORMAT_V = 3` / `DOC_SCHEMA_V = 1`: field 1 is `3`,
/// field 5 is the HLC array `[physical_ms, logical]`, field 12 is `1`, and the
/// magic prefix reads `5352 02 0003`.
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
        "000a57696e6e65722d6f702d63616e6f6e6963616c2d63626f720b58404e",
        "eb8efa26da3a3d01660956f68d48972a2599fa77660a0cc0046b475a210a",
        "ab8f256c45b80bc00c0502dae74ee117c8faa68d6f60733b8d003bdb2e37",
        "f968030c01",
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
        "550a58276416c4bb3e46b71d10c45af51e2462649e7331f6d5bbb8aba81d",
        "ff354b6cf029a0b818298dbaa40b5840c228871a4c8691ab88b7bfaad789",
        "b970d51862a3f59bcca561c9c31cfbea14bee09250a88e0331214ae50325",
        "0b4c9738b6ca4142b31b6de74efd9fa53ee75b020c01",
    ));
}
