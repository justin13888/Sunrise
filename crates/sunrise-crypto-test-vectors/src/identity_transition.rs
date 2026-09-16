//! The identity-rotation family, frozen: the device cert, the two HPKE share
//! `info` strings, and one canonical `identity_transition`.
//!
//! Specified by `docs/03-crypto/identity-and-device-keys.md` (the cert) and
//! `docs/03-crypto/key-rotation.md` §Identity rotation with ADR-0037 (the
//! transition). Asserted by `sunrise-crypto/tests/frozen_vectors.rs`.
//!
//! # Why one transition vector and not six domain vectors
//!
//! Four production domain strings live in this family:
//! `sunrise.identity_transition.v1`, `sunrise.identity_transition.succ.v1`,
//! `sunrise.identity_roster.v1` and `sunrise.identity_shares.v1`. Six separate
//! "hash this string" vectors would pin the four strings and nothing else —
//! and the strings are not what a second implementation has to get right. The
//! two signature domains do not exist on their own at all: they are prefixes
//! of a signature input over `body_hash`, and `body_hash` is BLAKE3 of the
//! body's canonical CBOR, whose field order is itself part of the format. A
//! vector that hashed the domain string in isolation would pass on a build
//! whose CBOR field names had moved.
//!
//! So the transition is frozen as **one body** — [`transition::BODY_CBOR`],
//! [`transition::BODY_HASH`], [`transition::PREV_SIG`],
//! [`transition::NEXT_SIG`] — which pins both signature domains, the CBOR
//! encoding, the hash, and the chaining rule that `next_sig` covers `prev_sig`,
//! in the one call graph production actually uses.
//!
//! The two digests are frozen **separately** beside it
//! ([`transition::ROSTER_DIGEST`], [`transition::SHARES_DIGEST`]), because
//! `roster_digest` and `shares_digest` are public functions in their own right
//! whose outputs merely feed the body. Keeping them as their own literals
//! localises a failure: a rename of `sunrise.identity_roster.v1` reddens the
//! roster assertion by name, rather than only saying "the transition vector
//! moved" and leaving a reader to find which of four inputs did it.
//!
//! Net: four domains, one body, six literals, four assertions.

/// Ed25519 secret seed of the **outgoing** account identity (`ID_S_priv`).
pub const IDENTITY_SIGNING_SECRET: [u8; 32] = [0x21; 32];

/// `ID_S_pub` for [`IDENTITY_SIGNING_SECRET`].
pub const IDENTITY_SIGNING_PUBLIC: [u8; 32] =
    crate::hex("884b8857f4eaa1613c61504db34d4beaf346517a0e31de3cddd4d9b4201d9d0b");

/// `identity_id_from_pub(IDENTITY_SIGNING_PUBLIC)`.
pub const IDENTITY_ID: [u8; 16] = crate::hex("45a0390256fa6254564d065f60299781");

/// X25519 secret of the outgoing account identity (`ID_D_priv`).
pub const IDENTITY_DH_SECRET: [u8; 32] = [0x41; 32];

/// `ID_D_pub` for [`IDENTITY_DH_SECRET`].
pub const IDENTITY_DH_PUBLIC: [u8; 32] =
    crate::hex("7a1a4e709bf085ac494aba0469b9b1eda0ab1f78b16aabb79ffeda90623e8522");

/// Ed25519 secret seed of the **successor** identity.
pub const SUCCESSOR_SIGNING_SECRET: [u8; 32] = [0x31; 32];

/// `to_id_s_pub` for [`SUCCESSOR_SIGNING_SECRET`].
pub const SUCCESSOR_SIGNING_PUBLIC: [u8; 32] =
    crate::hex("48075a597e721a156e2e0799de5cc0c5324dc6e7eaf1cdd46250868ec53215dd");

/// X25519 secret of the successor identity.
pub const SUCCESSOR_DH_SECRET: [u8; 32] = [0x51; 32];

/// `to_id_d_pub` for [`SUCCESSOR_DH_SECRET`].
pub const SUCCESSOR_DH_PUBLIC: [u8; 32] =
    crate::hex("ad908a8a708aca07588cda7c4ed3e44d4966a80a9abb2f1e4bbac53c67414e34");

/// `identity_id_from_pub(SUCCESSOR_SIGNING_PUBLIC)`.
pub const SUCCESSOR_IDENTITY_ID: [u8; 16] = crate::hex("284c6ded64230b663e86fc76156b088c");

/// A whole `DeviceCert`, frozen.
///
/// `sig = Ed25519(ID_S_priv, "sunrise.device_cert.v1" || BLAKE3(body_bytes))`
/// per `docs/03-crypto/identity-and-device-keys.md`. Ed25519 is deterministic,
/// so a fixed identity seed and a fixed body pin the signature exactly — and
/// the signature is the only thing the cert's domain string is visible in, so
/// this is the vector that anchors it.
///
/// [`ENCODED`](device_cert::ENCODED) is the outer CBOR `{1: body_bytes,
/// 2: sig}`. The body travels as an opaque `bstr` since `CRYPTO_SUITE_V = 5`,
/// which is why [`BODY_BYTES`](device_cert::BODY_BYTES) is frozen on its own:
/// it is the exact sequence the signature covers.
pub mod device_cert {
    /// `v` field.
    pub const V: u32 = 1;
    /// `device_id`, which is `device_id_from_pub(D_S_pub)` for
    /// [`crate::DEVICE_SIGNING_PUBLIC`].
    pub const DEVICE_ID: [u8; 16] = crate::hex("bd7a2df3f45482e111edeee969e0168e");
    /// `d_d_pub`, the X25519 public half of [`crate::key_envelope::RECIPIENT_SECRET`].
    pub const D_D_PUB: [u8; 32] = crate::key_envelope::RECIPIENT_PUBLIC;
    /// `created_at_ms`.
    pub const CREATED_AT_MS: u64 = 1_700_000_000_000;
    /// `nickname`.
    pub const NICKNAME: &str = "frozen-device";
    /// `platform`.
    pub const PLATFORM: &str = "frozen-platform";
    /// The canonical body encoding — the bytes the signature is taken over.
    pub const BODY_BYTES: [u8; 151] = crate::hex(concat!(
        "a801010250bd7a2df3f45482e111edeee969e0168e035820d04ab232742b",
        "b4ab3a1368bd4615e4e6d0224ab71a016baf8520a332c977873704582021",
        "9e4d800da968d2a5fcb009c784f4746c7138edb9ee4844b739e830b05cf4",
        "24055045a0390256fa6254564d065f60299781061b0000018bcfe5680007",
        "6d66726f7a656e2d646576696365086f66726f7a656e2d706c6174666f72",
        "6d",
    ));
    /// The Ed25519 signature over `"sunrise.device_cert.v1" || BLAKE3(body)`.
    pub const SIG: [u8; 64] = crate::hex(concat!(
        "19cbc90635270a6217dfd949ca175a27007bff507bc2e43483c0f7be7e05",
        "4dedce0f2010bbb9c1f9d6251ddec4979d540e8ec51f38ebc088ecc890c4",
        "c89fa50b",
    ));
    /// `to_cbor()` of the whole cert: `{1: BODY_BYTES, 2: SIG}`.
    pub const ENCODED: [u8; 222] = crate::hex(concat!(
        "a2015897a801010250bd7a2df3f45482e111edeee969e0168e035820d04a",
        "b232742bb4ab3a1368bd4615e4e6d0224ab71a016baf8520a332c9778737",
        "045820219e4d800da968d2a5fcb009c784f4746c7138edb9ee4844b739e8",
        "30b05cf424055045a0390256fa6254564d065f60299781061b0000018bcf",
        "e56800076d66726f7a656e2d646576696365086f66726f7a656e2d706c61",
        "74666f726d02584019cbc90635270a6217dfd949ca175a27007bff507bc2",
        "e43483c0f7be7e054dedce0f2010bbb9c1f9d6251ddec4979d540e8ec51f",
        "38ebc088ecc890c4c89fa50b",
    ));
}

/// The two `identity_transition` HPKE `info` strings, frozen as bytes.
///
/// Neither string is transmitted — HPKE binds `info` into the key schedule and
/// sends nothing about it — so two builds that construct them differently each
/// open their own shares perfectly and cannot open each other's. That is the
/// same reason [`crate::key_envelope::INFO`] is frozen, and a round-trip test
/// cannot substitute for it.
pub mod share_info {
    /// `to_identity_id` both strings bind.
    pub const TO_IDENTITY_ID: [u8; 16] = super::SUCCESSOR_IDENTITY_ID;
    /// The one device the per-device share is addressed to.
    pub const DEVICE_ID: [u8; 16] = super::device_cert::DEVICE_ID;
    /// `"sunrise.identity_share.v1" || to_identity_id || device_id`.
    pub const SHARE: [u8; 57] = crate::hex(concat!(
        "73756e726973652e6964656e746974795f73686172652e7631284c6ded64",
        "230b663e86fc76156b088cbd7a2df3f45482e111edeee969e0168e",
    ));
    /// `"sunrise.identity_carry.v1" || to_identity_id`.
    pub const CARRY: [u8; 41] = crate::hex(concat!(
        "73756e726973652e6964656e746974795f63617272792e7631284c6ded64",
        "230b663e86fc76156b088c",
    ));
}

/// One canonical `identity_transition`, frozen whole.
///
/// The roster is the single cert [`device_cert::ENCODED`]; the shares are one
/// fixed-width per-device share plus a carry-forward share, both filler bytes,
/// because `shares_digest` reads their lengths and their order and never their
/// contents.
pub mod transition {
    /// The single per-device share, `DEVICE_SHARE_LEN` bytes.
    pub const DEVICE_SHARE: [u8; 80] = [0x5a; 80];
    /// The carry-forward share, `IDENTITY_SHARE_LEN` bytes.
    pub const IDENTITY_SHARE: [u8; 112] = [0x6b; 112];
    /// `roster_digest(&[device_cert::ENCODED])` — anchors
    /// `sunrise.identity_roster.v1`.
    pub const ROSTER_DIGEST: [u8; 32] = crate::hex(concat!(
        "ef5936135d2a1254809a56a8a4973fd00180258ad69e693432d5c99d7ed7",
        "314b",
    ));
    /// `shares_digest(&[(device_cert::DEVICE_ID, DEVICE_SHARE)],
    /// Some(IDENTITY_SHARE))` — anchors `sunrise.identity_shares.v1`.
    pub const SHARES_DIGEST: [u8; 32] = crate::hex(concat!(
        "adc570aa73ef06c71216e2fb02d5234193c28271744ce0698cdbfa650a97",
        "6de8",
    ));
    /// `encode_canonical(body)`. Frozen because the field order here is the
    /// *encoded* order, which is not the declaration order, and both
    /// signatures are taken over its hash.
    pub const BODY_CBOR: [u8; 255] = crate::hex(concat!(
        "a66b746f5f69645f645f7075625820ad908a8a708aca07588cda7c4ed3e4",
        "4d4966a80a9abb2f1e4bbac53c67414e346b746f5f69645f735f70756258",
        "2048075a597e721a156e2e0799de5cc0c5324dc6e7eaf1cdd46250868ec5",
        "3215dd6d726f737465725f6469676573745820ef5936135d2a1254809a56",
        "a8a4973fd00180258ad69e693432d5c99d7ed7314b6d7368617265735f64",
        "69676573745820adc570aa73ef06c71216e2fb02d5234193c28271744ce0",
        "698cdbfa650a976de86e746f5f6964656e746974795f696450284c6ded64",
        "230b663e86fc76156b088c7066726f6d5f6964656e746974795f69645045",
        "a0390256fa6254564d065f60299781",
    ));
    /// `BLAKE3(BODY_CBOR)`.
    pub const BODY_HASH: [u8; 32] = crate::hex(concat!(
        "ba71e4a461c29222c1e4c8f4e3206c3245c2812f813189d213b2a55bf0e2",
        "4fa3",
    ));
    /// `Ed25519(outgoing ID_S_priv, "sunrise.identity_transition.v1" || BODY_HASH)`.
    pub const PREV_SIG: [u8; 64] = crate::hex(concat!(
        "89133d8e6115174ae40cf77f846626be1b444da9647aec1edcbea3d7f5c4",
        "c3e361d06e7870a31b2e16532b100078605f7c1ec7da469627235023298f",
        "441b5401",
    ));
    /// `Ed25519(successor ID_S_priv, "sunrise.identity_transition.succ.v1"
    /// || BODY_HASH || PREV_SIG)`.
    pub const NEXT_SIG: [u8; 64] = crate::hex(concat!(
        "5f86a1544936dada9a134cc3e1e998047ead1051b77a39939fb01d82f42b",
        "2ce841399257569c9092ed029ea8c3c34d9de26333a06adf30efe2f60e2b",
        "6d165309",
    ));
}
