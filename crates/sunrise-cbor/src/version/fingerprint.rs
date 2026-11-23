//! The document-schema fingerprint registry (ADR-0045 §2–§3): every
//! fingerprinted [`DOC_SCHEMA_V`] to its fingerprint, and the lookups the
//! envelope writer and receiver make against it.
//!
//! Split out of [`super`] because it is the one part of the version constants
//! that grows with every document-schema bump, by an appended entry, and
//! shares nothing else with the other constants there.

use super::DOC_SCHEMA_V;

/// The first [`DOC_SCHEMA_V`] that has a fingerprint (ADR-0045 §3, `N_fp`).
///
/// An envelope below it carries no field 13 and is read under the legacy
/// rules. A writer at or above it MUST emit field 13.
pub const DOC_SCHEMA_FP_FIRST: u16 = 7;

// The build's own version is fingerprinted, so its writer can stamp field 13.
const _: () = assert!(DOC_SCHEMA_V >= DOC_SCHEMA_FP_FIRST);

/// BLAKE3 `derive_key` context for the document-schema fingerprint
/// (ADR-0045 §2): `fp = BLAKE3::derive_key(this, JCS(schema))`.
pub const DOC_SCHEMA_FP_DOMAIN: &str = "sunrise.doc_schema.fingerprint.v1";

/// How many leading bytes of the fingerprint envelope field 13 carries.
pub const DOC_SCHEMA_FP_PREFIX_LEN: usize = 8;

/// Every document-schema version that has a fingerprint, to that fingerprint,
/// in version order. **Append-only.**
///
/// This is the build's registry: the writer stamps envelope field 13 from it,
/// and a receiver compares field 13 against it. It is committed as
/// `schemas/doc-schema/registry.json`, and each entry is frozen as a literal
/// in `sunrise-crypto-test-vectors` (`protocol::DOC_SCHEMA_REGISTRY`), so an
/// entry that is edited or removed fails a test. An entry is what every build
/// that shipped it believes its version means; changing one makes two builds
/// disagree while their version numbers say they agree, which is the failure
/// the fingerprint exists to catch.
pub const DOC_SCHEMA_FINGERPRINTS: &[(u16, [u8; 32])] = &[
    (
        7,
        hex32("fb893b62bb2f9bf7d9adf7ba95d5bee20498a63aa0e03c4f14a7a28a7d0d6fcb"),
    ),
    (
        8,
        hex32("69a555bc13a1b05143608bf16370bb6fcdb0268e514efdbc30df070049dbf58d"),
    ),
    (
        9,
        hex32("875fc9a8b8d426ff9c52dd6fa5ce2da8685903353fc58f98446c4825c2d7de01"),
    ),
    (
        10,
        hex32("e9a6d82f484ef86b75291f8d58f28e1a04bc6ee5f877de89da5afe0af03ff12c"),
    ),
    (
        11,
        hex32("3c75fc18c3369d0481d746a3534a34028ad1068a02ee6816e3062b830e6debd4"),
    ),
    (
        12,
        hex32("1be56767d868335916e893ff50e7a6097d942957f1687109b8cc17fb4e2176f1"),
    ),
    (
        13,
        hex32("09ede17ec2962b1bc05b1214681c7fc1f4af325955535c27119d709c35a2cf05"),
    ),
    (
        14,
        hex32("300fb530ec0d3b3c3e5e24ed878fb9cc866cb4f2a3496bd37388369887f3749b"),
    ),
    (
        15,
        hex32("953c59ed8729b185b79ca821da37d40facc115054b8c11150d19d8e0b6999de4"),
    ),
    (
        16,
        hex32("ac96923d0675fabe2162d286f5ba5af7cb15cc4c32a4311bcc36e2fa6ad6c9ef"),
    ),
    (
        17,
        hex32("2fc9c2a11793f1e1940133d9d5949ddff676fa2cb73fbfd65f2175c0215a2531"),
    ),
    (
        18,
        hex32("4093cc32794e193cf86235c9d79e31437b4e46f694f9b419e7f9414845617015"),
    ),
    (
        19,
        hex32("46ee3fe735d8a27674e96e7ea8962031c544c790aba22921c260dca4afee084e"),
    ),
    (
        20,
        hex32("7f0c318d6bb81e2f7d2c755d5271f3c229854133bbe21c1dbcf0a0da24979fc9"),
    ),
    (
        21,
        hex32("35b2c6020668ba962d8f799f533beebf2c3ee14d0237a05734b6cad8bb50cebe"),
    ),
    (
        22,
        hex32("878b79742cd6b1884edbbdc4977f89c29391aebff25cb005d9e1cfcaae9a9454"),
    ),
    (
        23,
        hex32("7f0884f51ccdb23cf36769d87c42284780101d1a4ddd0bee5afedcd78437be56"),
    ),
    (
        24,
        hex32("38b444fd794761d78a30a88636ba3d97fbe997c2152ff079db31e176013cdf67"),
    ),
    (
        25,
        hex32("c963f3ddbe7a950b0de0689ce797ae92f62c2ba0931e688926dcb45affd4d041"),
    ),
    (
        26,
        hex32("9dd27252fdf8a718cd9c7c956e2e3f02db6965debe1a4fd3b05d34aabf5b5bdd"),
    ),
    (
        27,
        hex32("13933824b64ec592adc2f55fa355339b331e8661fb7b9cc159444107289396f8"),
    ),
    (
        28,
        hex32("66507bdffc4e9ba02fe2fc0857d1fefd0c3815af2495b455180e0508fe602979"),
    ),
    (
        29,
        hex32("3602cfdc5e1d3bf31bc5948d33e3b89decf056299021c75c329b2f727dd5830d"),
    ),
    (
        30,
        hex32("5b3ed22a960742f10993af4cc6396424c46f4234a1ee1e5a207bc5856dd6e8e0"),
    ),
    (
        31,
        hex32("8594c078d9f1afcc5e9e6b430b6220b3e1a68b6064a050599b093ff81799539e"),
    ),
    (
        32,
        hex32("ec0dc7290af1db0ecfc1501eaf2ad6f31a9139d68b80c02bec4ee163f2f0a474"),
    ),
    (
        33,
        hex32("6c7c0cb5311f7c7a64f148aff800c1b7402bfe5536fb16f912c3d78074201ea5"),
    ),
    (
        34,
        hex32("3e1717301ffda3a0c63af64bb36570d1b08e6225f8a447fb3232ef53afd6d6ab"),
    ),
];

/// The registered fingerprint of document schema `v`, or `None` for a version
/// this build has no entry for: one before [`DOC_SCHEMA_FP_FIRST`], or one
/// newer than this build.
#[must_use]
pub fn doc_schema_fingerprint(v: u32) -> Option<[u8; 32]> {
    DOC_SCHEMA_FINGERPRINTS
        .iter()
        .find(|(known, _)| u32::from(*known) == v)
        .map(|(_, fp)| *fp)
}

/// What envelope field 13 carries for document schema `v`: the first
/// [`DOC_SCHEMA_FP_PREFIX_LEN`] bytes of its registered fingerprint, or `None`
/// where [`doc_schema_fingerprint`] has no entry.
#[must_use]
pub fn doc_schema_fp_prefix(v: u32) -> Option<[u8; DOC_SCHEMA_FP_PREFIX_LEN]> {
    doc_schema_fingerprint(v).map(|fp| {
        let mut prefix = [0u8; DOC_SCHEMA_FP_PREFIX_LEN];
        prefix.copy_from_slice(&fp[..DOC_SCHEMA_FP_PREFIX_LEN]);
        prefix
    })
}

/// Decode 64 lowercase hex digits at compile time. A wrong length or digit in
/// a registry entry is a compile error.
pub(super) const fn hex32(s: &str) -> [u8; 32] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("registry fingerprints are lowercase hex"),
        }
    }
    let b = s.as_bytes();
    assert!(b.len() == 64, "a fingerprint is 64 hex digits");
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = (nibble(b[2 * i]) << 4) | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}
