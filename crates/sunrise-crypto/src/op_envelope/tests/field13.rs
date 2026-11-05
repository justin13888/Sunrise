//! Field 13, the document schema's fingerprint (ADR-0045 §3).

use super::*;

/// ADR-0045 §3: a writer at a fingerprinted version emits field 13, and it
/// holds the first 8 bytes of that version's registered fingerprint.
#[test]
fn the_writer_stamps_field_13_with_its_registered_fingerprint() {
    use sunrise_cbor::version::doc_schema_fingerprint;
    let signing = fixed_signing();
    let bytes = encode_envelope(
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
    let env = decode_envelope(&bytes).unwrap();
    let registered = doc_schema_fingerprint(u32::from(DOC_SCHEMA_V)).expect("registered");
    assert_eq!(
        env.schema_fp.map(|fp| fp.to_vec()),
        Some(registered[..8].to_vec())
    );
    assert!(env.unknown.is_empty(), "field 13 is known, not preserved");

    // `0d 48 <8 bytes>`: field 13 as an 8-byte byte string, last in the map.
    let mut tail = vec![0x0d, 0x48];
    tail.extend_from_slice(&registered[..8]);
    assert!(contains_subseq(&bytes, &tail));
}

/// An envelope from before the fingerprint has no field 13, and still
/// decodes and verifies.
#[test]
fn an_envelope_without_field_13_decodes_with_none() {
    let signing = fixed_signing();
    let bytes = envelope_at_doc_schema(6, &signing);
    let env = decode_envelope(&bytes).unwrap();
    assert_eq!(env.schema_fp, None);
    verify_envelope(&env, &signing.public_bytes()).unwrap();
}

/// Field 13 is inside the signature input and the AEAD associated data,
/// by the exclusion rule: a relay can neither strip nor rewrite it.
#[test]
fn field_13_is_covered_by_the_signature_and_the_aad() {
    let signing = fixed_signing();
    let pub_bytes = signing.public_bytes();
    let stream_key = fixed_stream_key();
    let bytes = seal_envelope(
        OpEnvelope {
            v: u32::from(ENVELOPE_FORMAT_V),
            doc_schema_v: u32::from(DOC_SCHEMA_V),
            schema_fp: Some([0xab; 8]),
            stream_id: [2u8; 16],
            device_id: [3u8; 16],
            seq: 1,
            hlc: Hlc::at(1),
            aead_alg: AeadAlgId::XChaCha20Poly1305,
            sig_alg: SigAlgId::Ed25519,
            epoch: 1,
            nonce: [0u8; AEAD_NONCE_LEN],
            payload: b"secret".to_vec(),
            sig: [0u8; 64],
            prev_hash: None,
            heads: Vec::new(),
            unknown: BTreeMap::new(),
        },
        Some(&stream_key),
        &signing,
    )
    .unwrap();
    let env = decode_envelope(&bytes).unwrap();
    assert_eq!(env.schema_fp, Some([0xab; 8]));
    open_envelope(&env, &pub_bytes, Some(&stream_key)).unwrap();

    for tampered in [Some([0xcd; 8]), None] {
        let mut env = env.clone();
        env.schema_fp = tampered;
        assert!(
            matches!(
                verify_envelope(&env, &pub_bytes),
                Err(OpEnvelopeError::SigVerify)
            ),
            "{tampered:?} passed the signature"
        );
        sign_envelope(&mut env, &signing).unwrap();
        assert!(
            matches!(
                open_envelope(&env, &pub_bytes, Some(&stream_key)),
                Err(OpEnvelopeError::AeadAuth)
            ),
            "{tampered:?} passed the AAD"
        );
    }
}

/// Field 13 has one shape. Anything else is a malformed envelope, refused
/// like a wrong-sized `stream_id`, rather than kept as an unknown.
#[test]
fn a_field_13_of_the_wrong_shape_is_refused() {
    let signing = fixed_signing();
    let bytes = envelope_at_doc_schema(1, &signing);
    let mut value: ciborium::value::Value = ciborium::de::from_reader(&bytes[MAGIC_LEN..]).unwrap();
    for wrong in [
        ciborium::value::Value::Bytes(vec![0; 7]),
        ciborium::value::Value::Bytes(vec![0; 32]),
        ciborium::value::Value::Integer(13.into()),
    ] {
        if let ciborium::value::Value::Map(m) = &mut value {
            m.retain(
                |(k, _)| !matches!(k, ciborium::value::Value::Integer(i) if i128::from(*i) == 13),
            );
            m.push((ciborium::value::Value::Integer(13.into()), wrong));
        }
        let mut out = bytes[..MAGIC_LEN].to_vec();
        ciborium::ser::into_writer(&value, &mut out).unwrap();
        assert!(matches!(
            decode_envelope(&out),
            Err(OpEnvelopeError::BadField("schema_fp"))
        ));
    }
}
