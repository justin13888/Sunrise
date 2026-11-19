use super::*;

/// v22's registered fingerprint, written out byte by byte rather than
/// decoded, so the lookup is checked against something `hex32` did not
/// produce. Its hex form uses every digit `0`-`9` and `a`-`f`.
const V22: [u8; 32] = [
    0x87, 0x8b, 0x79, 0x74, 0x2c, 0xd6, 0xb1, 0x88, 0x4e, 0xdb, 0xbd, 0xc4, 0x97, 0x7f, 0x89, 0xc2,
    0x93, 0x91, 0xae, 0xbf, 0xf2, 0x5c, 0xb0, 0x05, 0xd9, 0xe1, 0xcf, 0xca, 0xae, 0x9a, 0x94, 0x54,
];

#[test]
fn hex32_decodes_every_digit_high_nibble_first() {
    let decoded = hex32("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    let row = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
    for (i, byte) in decoded.iter().enumerate() {
        assert_eq!(*byte, row[i % row.len()], "byte {i}");
    }
}

#[test]
fn a_registered_version_finds_its_own_fingerprint() {
    assert_eq!(doc_schema_fingerprint(22), Some(V22));
    // The first entry too, so a lookup that always answered with the last
    // one would not pass.
    let (first, first_fp) = DOC_SCHEMA_FINGERPRINTS[0];
    assert_eq!(first, DOC_SCHEMA_FP_FIRST);
    assert_eq!(doc_schema_fingerprint(u32::from(first)), Some(first_fp));
    assert_ne!(first_fp, V22);
}

#[test]
fn an_unregistered_version_has_no_fingerprint() {
    let last = DOC_SCHEMA_FINGERPRINTS[DOC_SCHEMA_FINGERPRINTS.len() - 1].0;
    for v in [
        0,
        u32::from(DOC_SCHEMA_FP_FIRST) - 1,
        u32::from(last) + 1,
        u32::MAX,
    ] {
        assert_eq!(doc_schema_fingerprint(v), None, "v{v}");
        assert_eq!(doc_schema_fp_prefix(v), None, "v{v}");
    }
}

#[test]
fn the_prefix_is_the_fingerprints_leading_bytes() {
    assert_eq!(
        doc_schema_fp_prefix(22),
        Some([0x87, 0x8b, 0x79, 0x74, 0x2c, 0xd6, 0xb1, 0x88])
    );
}
