//! Seal, decode, verify and open, and the gates that refuse a tampered or
//! foreign envelope.

use super::*;

#[test]
fn signed_only_round_trip() {
    let signing = fixed_signing();
    let pub_bytes = signing.public_bytes();
    let inner = b"hello inner op";
    let bytes = encode_envelope(
        inner,
        [2u8; 16],
        [3u8; 16],
        1,
        Hlc::at(1_700_000_000_000),
        AeadAlgId::None,
        0,
        [0u8; AEAD_NONCE_LEN],
        None,
        &signing,
    )
    .unwrap();
    // Magic prefix carries ENVELOPE_FORMAT_V: `SR\x02\x00\x03`.
    assert_eq!(&bytes[..MAGIC_LEN], b"SR\x02\x00\x03");
    let env = decode_envelope(&bytes).unwrap();
    verify_envelope(&env, &pub_bytes).unwrap();
    let plaintext = open_envelope(&env, &pub_bytes, None).unwrap();
    assert_eq!(&plaintext, inner);
}

#[test]
fn aead_round_trip() {
    let signing = fixed_signing();
    let pub_bytes = signing.public_bytes();
    let stream_key = fixed_stream_key();
    let inner = b"sensitive payload";
    let bytes = encode_envelope(
        inner,
        [2u8; 16],
        [3u8; 16],
        1,
        Hlc::at(1_700_000_000_000),
        AeadAlgId::XChaCha20Poly1305,
        4,
        [0xaau8; AEAD_NONCE_LEN],
        Some(&stream_key),
        &signing,
    )
    .unwrap();
    let env = decode_envelope(&bytes).unwrap();
    // Payload field is encrypted; should not contain `inner` verbatim.
    assert!(!contains_subseq(&env.payload, inner));
    let plaintext = open_envelope(&env, &pub_bytes, Some(&stream_key)).unwrap();
    assert_eq!(&plaintext, inner);
}

/// The relay routes by `(stream_id, device_id, seq)` using a header-only
/// reader that links no crypto at all. If the two decoders ever disagreed
/// about where those ids live, the relay would filter the wrong frames —
/// so a real sealed envelope is checked against both here, in the crate
/// that owns the format.
#[test]
fn header_only_reader_agrees_with_the_full_decoder() {
    let signing = fixed_signing();
    let stream_key = fixed_stream_key();
    let bytes = encode_envelope(
        b"opaque to a relay",
        [0x5au8; 16],
        [0xc3u8; 16],
        77,
        Hlc::at(1_700_000_000_000),
        AeadAlgId::XChaCha20Poly1305,
        4,
        [0xaau8; AEAD_NONCE_LEN],
        Some(&stream_key),
        &signing,
    )
    .unwrap();
    let full = decode_envelope(&bytes).unwrap();
    let head = sunrise_cbor::decode_envelope_header(&bytes).unwrap();
    assert_eq!(head.stream_id, full.stream_id);
    assert_eq!(head.device_id, full.device_id);
    assert_eq!(head.seq, full.seq);
}

#[test]
fn sig_failure_rejects_before_decrypt() {
    let signing = fixed_signing();
    let stream_key = fixed_stream_key();
    let mut bytes = encode_envelope(
        b"x",
        [2u8; 16],
        [3u8; 16],
        1,
        Hlc::at(1),
        AeadAlgId::XChaCha20Poly1305,
        1,
        [0u8; AEAD_NONCE_LEN],
        Some(&stream_key),
        &signing,
    )
    .unwrap();
    // Flip a byte in the signature region. Field 11 is no longer last —
    // field 12 (`doc_schema_v`) encodes as the trailing `0c 01` — so step
    // back past it to land inside the 64-byte signature.
    let in_sig = bytes.len() - 3;
    bytes[in_sig] ^= 0x01;
    let env = decode_envelope(&bytes).unwrap();
    let bogus_pub = [9u8; 32];
    assert!(matches!(
        verify_envelope(&env, &bogus_pub),
        Err(OpEnvelopeError::SigVerify)
    ));
}

#[test]
fn aead_tamper_rejects() {
    let signing = fixed_signing();
    let pub_bytes = signing.public_bytes();
    let stream_key = fixed_stream_key();
    let bytes = encode_envelope(
        b"x",
        [2u8; 16],
        [3u8; 16],
        1,
        Hlc::at(1),
        AeadAlgId::XChaCha20Poly1305,
        1,
        [0u8; AEAD_NONCE_LEN],
        Some(&stream_key),
        &signing,
    )
    .unwrap();
    let mut env = decode_envelope(&bytes).unwrap();
    // Tampering ciphertext invalidates AEAD; signature still passes
    // because we re-sign below, simulating a malicious peer who knows
    // the device signing key (they shouldn't, but we test the AEAD
    // gate independently).
    if let Some(b) = env.payload.first_mut() {
        *b ^= 0x01;
    }
    sign_envelope(&mut env, &signing).unwrap();
    assert!(matches!(
        open_envelope(&env, &pub_bytes, Some(&stream_key)),
        Err(OpEnvelopeError::AeadAuth)
    ));
}

#[test]
fn bad_magic_rejected() {
    let signing = fixed_signing();
    let mut bytes = encode_envelope(
        b"x",
        [2u8; 16],
        [3u8; 16],
        1,
        Hlc::at(1),
        AeadAlgId::None,
        0,
        [0u8; AEAD_NONCE_LEN],
        None,
        &signing,
    )
    .unwrap();
    bytes[0] = b'X';
    assert!(matches!(
        decode_envelope(&bytes),
        Err(OpEnvelopeError::BadMagic)
    ));
}
