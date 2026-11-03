//! The container format (field 1 and the prefix floor) and the document
//! schema (field 12): separate versions, each with its own floor.

use super::*;

#[test]
fn envelope_carries_both_versions_separately() {
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
    assert_eq!(env.v, u32::from(ENVELOPE_FORMAT_V));
    assert_eq!(env.doc_schema_v, u32::from(DOC_SCHEMA_V));
}

/// The point of the split. A build that has never heard of doc schema 99
/// still decodes, authenticates, and opens the envelope — it only cannot
/// interpret whatever new fields the *payload* carries.
#[test]
fn newer_doc_schema_still_decodes_and_verifies() {
    let signing = fixed_signing();
    let pub_bytes = signing.public_bytes();
    let bytes = envelope_at_doc_schema(99, &signing);
    let env = decode_envelope(&bytes).expect("a newer doc schema is readable");
    assert_eq!(env.doc_schema_v, 99);
    assert_eq!(env.v, u32::from(ENVELOPE_FORMAT_V));
    let plaintext = open_envelope(&env, &pub_bytes, None).unwrap();
    assert_eq!(plaintext, b"inner");
}

/// ...but a schema below the floor is refused, with its own error rather
/// than the container-format one.
#[test]
fn doc_schema_below_floor_is_refused() {
    let signing = fixed_signing();
    let bytes = envelope_at_doc_schema(u32::from(DOC_SCHEMA_FLOOR) - 1, &signing);
    assert!(matches!(
        decode_envelope(&bytes),
        Err(OpEnvelopeError::DocSchemaTooOld { got: 0, floor: 1 })
    ));
}

/// A container-format mismatch is fatal in the prefix, before any CBOR is
/// parsed — the decoder does not know the layout, so it cannot find a
/// payload to salvage.
#[test]
fn foreign_envelope_format_is_rejected_at_the_prefix() {
    let signing = fixed_signing();
    let mut bytes = envelope_at_doc_schema(1, &signing);
    // Bump the prefix's version word to a format this build has never seen.
    bytes[3] = 0xff;
    bytes[4] = 0xff;
    assert!(matches!(
        decode_envelope(&bytes),
        Err(OpEnvelopeError::BadMagic)
    ));
}

/// Field 1 is the writer's own container, so it cannot be older than the
/// floor the same writer stamped in the prefix (ADR-0045 §5).
#[test]
fn a_container_below_its_own_floor_is_refused() {
    let signing = fixed_signing();
    let mut env = decode_envelope(&envelope_at_doc_schema(1, &signing)).unwrap();
    env.v = u32::from(ENVELOPE_FORMAT_FLOOR) - 1;
    let bytes = seal_envelope(env, None, &signing).unwrap();
    assert!(matches!(
        decode_envelope(&bytes),
        Err(OpEnvelopeError::BadField("v"))
    ));
}

/// ADR-0045 §5: a change to the meaning of fields 1–12, to the AAD
/// construction or to the signature input MUST raise
/// [`ENVELOPE_FORMAT_FLOOR`]. Each of the three is pinned here to the
/// floor it was frozen at, so changing one fails this test until the
/// floor moves, and the floor moving is what tells this test to re-pin.
#[test]
fn the_container_meaning_is_pinned_to_its_floor() {
    /// The floor everything below was frozen at.
    const PINNED_FLOOR: u16 = 3;
    assert_eq!(
        ENVELOPE_FORMAT_FLOOR, PINNED_FLOOR,
        "the floor moved: re-pin fields 1-12, the Omit sets and SIG_DOMAIN \
         to the new container, then PINNED_FLOOR"
    );

    // The signature input's domain.
    assert_eq!(SIG_DOMAIN, b"sunrise.op_envelope.v1");

    // The AAD and signature inputs, by exclusion.
    for id in 0..=u8::MAX {
        assert!(!Omit::Nothing.omits(id), "full encoding omits {id}");
        assert_eq!(Omit::Sig.omits(id), id == 11, "signature input, field {id}");
        assert_eq!(
            Omit::PayloadAndSig.omits(id),
            id == 10 || id == 11,
            "AAD, field {id}"
        );
    }

    // Fields 1–12: their ids, their order and their encodings, from an
    // envelope whose every field holds a distinct value.
    let env = OpEnvelope {
        v: 3,
        stream_id: [0x02; 16],
        device_id: [0x03; 16],
        seq: 4,
        hlc: Hlc {
            physical_ms: 5,
            logical: 55,
        },
        aead_alg: AeadAlgId::XChaCha20Poly1305,
        sig_alg: SigAlgId::Ed25519,
        epoch: 8,
        nonce: [0x09; AEAD_NONCE_LEN],
        payload: vec![0x0a; 3],
        sig: [0x0b; 64],
        doc_schema_v: 12,
        schema_fp: None,
        prev_hash: None,
        heads: Vec::new(),
        unknown: BTreeMap::new(),
    };
    let cbor = encode_cbor(&env, Omit::Nothing).unwrap();
    // One line per field: its id, then its encoded value.
    let pinned = concat!(
        "ac",                                                     // map(12)
        "0103",                                                   // 1: v
        "025002020202020202020202020202020202",                   // 2: stream_id
        "035003030303030303030303030303030303",                   // 3: device_id
        "0404",                                                   // 4: seq
        "0582051837",                                             // 5: hlc [5, 55]
        "0601",                                                   // 6: aead_alg
        "0701",                                                   // 7: sig_alg
        "0808",                                                   // 8: epoch
        "095818090909090909090909090909090909090909090909090909", // 9: nonce
        "0a430a0a0a",                                             // 10: payload
        "0b5840",                                                 // 11: sig, 64 x 0x0b
        "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b",
        "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b",
        "0c0c", // 12: doc_schema_v
    );
    assert_eq!(hex::encode(&cbor), pinned, "fields 1-12 changed encoding");

    // ...and read back to the same meaning.
    let mut framed = vec![0u8; MAGIC_LEN];
    write_prefix(&mut framed, MagicKind::OpEnvelope, PINNED_FLOOR);
    framed.extend_from_slice(&cbor);
    assert_eq!(decode_envelope(&framed).unwrap(), env);
}

/// The transition rule of ADR-0045 §5. Every build before the floor
/// compares the prefix and field 1 with its own container for equality,
/// so a writer that stamps a newer field 1 strands all of them. That stays
/// forbidden until `core.envelope_floor` is in `vault_requires` and the
/// relay agrees `SrvEnvelopeFloor`, neither of which exists yet.
#[test]
fn writers_still_emit_the_container_every_build_reads() {
    assert_eq!(ENVELOPE_FORMAT_V, ENVELOPE_FORMAT_FLOOR);
    let signing = fixed_signing();
    let bytes = envelope_at_doc_schema(1, &signing);
    assert_eq!(&bytes[..MAGIC_LEN], b"SR\x02\x00\x03");
    assert_eq!(decode_envelope(&bytes).unwrap().v, 3);
}

/// Field 12 rides inside the signature input. Rewriting it therefore
/// invalidates the signature rather than passing unnoticed.
#[test]
fn doc_schema_is_covered_by_the_signature() {
    let signing = fixed_signing();
    let pub_bytes = signing.public_bytes();
    let bytes = envelope_at_doc_schema(1, &signing);
    let mut env = decode_envelope(&bytes).unwrap();
    verify_envelope(&env, &pub_bytes).unwrap();
    env.doc_schema_v = 2;
    assert!(matches!(
        verify_envelope(&env, &pub_bytes),
        Err(OpEnvelopeError::SigVerify)
    ));
}

/// ...and inside the AEAD associated data, so it cannot be rewritten by
/// anyone who does not hold the Stream key either.
#[test]
fn doc_schema_is_covered_by_the_aad() {
    let signing = fixed_signing();
    let pub_bytes = signing.public_bytes();
    let stream_key = fixed_stream_key();
    let bytes = seal_envelope(
        OpEnvelope {
            v: u32::from(ENVELOPE_FORMAT_V),
            doc_schema_v: 1,
            schema_fp: None,
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
    let mut env = decode_envelope(&bytes).unwrap();
    env.doc_schema_v = 7;
    // Re-sign, so the signature gate cannot be what rejects it.
    sign_envelope(&mut env, &signing).unwrap();
    assert!(matches!(
        open_envelope(&env, &pub_bytes, Some(&stream_key)),
        Err(OpEnvelopeError::AeadAuth)
    ));
}

#[test]
fn missing_doc_schema_field_is_refused() {
    // A well-formed v2 container MUST carry field 12; a map without it is
    // not a v2 envelope, whatever its prefix claims.
    let signing = fixed_signing();
    let bytes = envelope_at_doc_schema(1, &signing);
    let mut value: ciborium::value::Value = ciborium::de::from_reader(&bytes[MAGIC_LEN..]).unwrap();
    if let ciborium::value::Value::Map(m) = &mut value {
        m.retain(|(k, _)| !matches!(k, ciborium::value::Value::Integer(i) if i128::from(*i) == 12));
    }
    let mut out = bytes[..MAGIC_LEN].to_vec();
    ciborium::ser::into_writer(&value, &mut out).unwrap();
    assert!(matches!(
        decode_envelope(&out),
        Err(OpEnvelopeError::BadField("doc_schema_v"))
    ));
}
