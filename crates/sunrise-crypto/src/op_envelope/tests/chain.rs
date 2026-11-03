//! The chain fields 14 and 15 (ADR-0043 §1–§2), and the [`op_hash`] they
//! carry.

use super::*;

fn chained(chain: ChainLinks, signing: &DeviceSigningKeyPair) -> Vec<u8> {
    encode_chained_envelope(
        b"x",
        [2u8; 16],
        [3u8; 16],
        2,
        Hlc::at(1),
        AeadAlgId::XChaCha20Poly1305,
        1,
        [0u8; AEAD_NONCE_LEN],
        chain,
        Some(&fixed_stream_key()),
        signing,
    )
    .unwrap()
}

fn head(device: u8, seq: u64) -> ChainHead {
    ChainHead {
        device_id: [device; 16],
        seq,
        op_hash: [device ^ 0xff; 32],
    }
}

/// ADR-0043 §1–§2: fields 14 and 15 round-trip, sit after field 13, and
/// are known fields rather than preserved unknowns.
#[test]
fn chain_fields_round_trip_as_known_fields() {
    let signing = fixed_signing();
    let chain = ChainLinks {
        prev_hash: Some([0x14; 32]),
        heads: vec![head(1, 4), head(9, 2)],
    };
    let bytes = chained(chain.clone(), &signing);
    let env = decode_envelope(&bytes).unwrap();
    assert_eq!(env.prev_hash, chain.prev_hash);
    assert_eq!(env.heads, chain.heads);
    assert!(env.unknown.is_empty());
    open_envelope(&env, &signing.public_bytes(), Some(&fixed_stream_key())).unwrap();

    // `0e 58 20 <32 bytes>`: field 14 as a 32-byte byte string.
    let mut f14 = vec![0x0e, 0x58, 0x20];
    f14.extend_from_slice(&[0x14; 32]);
    assert!(contains_subseq(&bytes, &f14));
}

/// An envelope with no chain fields is the legacy link, and the chained
/// writer with an empty chain is byte-identical to the plain one.
#[test]
fn an_empty_chain_writes_neither_field() {
    let signing = fixed_signing();
    let env = decode_envelope(&chained(ChainLinks::default(), &signing)).unwrap();
    assert_eq!(env.prev_hash, None);
    assert!(env.heads.is_empty());
    let plain = encode_envelope(
        b"x",
        [2u8; 16],
        [3u8; 16],
        2,
        Hlc::at(1),
        AeadAlgId::XChaCha20Poly1305,
        1,
        [0u8; AEAD_NONCE_LEN],
        Some(&fixed_stream_key()),
        &signing,
    )
    .unwrap();
    assert_eq!(plain, chained(ChainLinks::default(), &signing));
}

/// Both fields are inside the signature input and the AAD by the
/// exclusion rule: a relay can neither strip nor forge a link.
#[test]
fn chain_fields_are_covered_by_the_signature_and_the_aad() {
    let signing = fixed_signing();
    let pub_bytes = signing.public_bytes();
    let key = fixed_stream_key();
    let env = decode_envelope(&chained(
        ChainLinks {
            prev_hash: Some([0x14; 32]),
            heads: vec![head(1, 4)],
        },
        &signing,
    ))
    .unwrap();
    let tampers: [fn(&mut OpEnvelope); 4] = [
        |e| e.prev_hash = None,
        |e| e.prev_hash = Some([0x15; 32]),
        |e| e.heads.clear(),
        |e| e.heads[0].seq += 1,
    ];
    for (i, tamper) in tampers.iter().enumerate() {
        let mut t = env.clone();
        tamper(&mut t);
        assert!(
            matches!(
                verify_envelope(&t, &pub_bytes),
                Err(OpEnvelopeError::SigVerify)
            ),
            "tamper {i} passed the signature"
        );
        sign_envelope(&mut t, &signing).unwrap();
        assert!(
            matches!(
                open_envelope(&t, &pub_bytes, Some(&key)),
                Err(OpEnvelopeError::AeadAuth)
            ),
            "tamper {i} passed the AAD"
        );
    }
}

/// Field 15 has one encoding per set: unsorted, duplicated, empty or
/// oversized lists are refused by the writer and by the reader alike.
#[test]
fn heads_out_of_shape_are_refused_both_ways() {
    let signing = fixed_signing();
    let too_many: Vec<ChainHead> = (0..=MAX_CHAIN_HEADS)
        .map(|i| {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&(i as u64).to_be_bytes());
            ChainHead {
                device_id: id,
                seq: 1,
                op_hash: [0; 32],
            }
        })
        .collect();
    for heads in [
        vec![head(9, 1), head(1, 1)],
        vec![head(1, 1), head(1, 2)],
        too_many,
    ] {
        let env = OpEnvelope {
            heads: heads.clone(),
            ..decode_envelope(&chained(ChainLinks::default(), &signing)).unwrap()
        };
        assert!(matches!(
            seal_envelope(env.clone(), Some(&fixed_stream_key()), &signing),
            Err(OpEnvelopeError::BadField("heads"))
        ));
        // The reader, fed the same list by hand.
        let mut cbor = encode_cbor(&env, Omit::Nothing).unwrap();
        let mut framed = vec![0u8; MAGIC_LEN];
        write_prefix(&mut framed, MagicKind::OpEnvelope, ENVELOPE_FORMAT_FLOOR);
        framed.append(&mut cbor);
        assert!(matches!(
            decode_envelope(&framed),
            Err(OpEnvelopeError::BadField("heads"))
        ));
    }

    // An empty array on the wire, and a malformed entry.
    let base = chained(ChainLinks::default(), &signing);
    let mut value: ciborium::value::Value = ciborium::de::from_reader(&base[MAGIC_LEN..]).unwrap();
    for wrong in [
        ciborium::value::Value::Array(vec![]),
        ciborium::value::Value::Array(vec![ciborium::value::Value::Integer(1.into())]),
        ciborium::value::Value::Bytes(vec![0; 32]),
    ] {
        if let ciborium::value::Value::Map(m) = &mut value {
            m.retain(
                |(k, _)| !matches!(k, ciborium::value::Value::Integer(i) if i128::from(*i) == 15),
            );
            m.push((ciborium::value::Value::Integer(15.into()), wrong));
        }
        let mut out = base[..MAGIC_LEN].to_vec();
        ciborium::ser::into_writer(&value, &mut out).unwrap();
        assert!(matches!(
            decode_envelope(&out),
            Err(OpEnvelopeError::BadField("heads"))
        ));
    }
}

/// `op_hash` covers the signature (field 11) and every header field, and
/// is the hash of the canonical encoding without the magic prefix.
#[test]
fn op_hash_is_the_hash_of_the_full_canonical_encoding() {
    let signing = fixed_signing();
    let bytes = chained(ChainLinks::default(), &signing);
    let env = decode_envelope(&bytes).unwrap();
    assert_eq!(
        op_hash(&env).unwrap(),
        *blake3::hash(&bytes[MAGIC_LEN..]).as_bytes()
    );
    let mut resigned = env.clone();
    resigned.sig[0] ^= 1;
    assert_ne!(op_hash(&env).unwrap(), op_hash(&resigned).unwrap());
    let mut relinked = env.clone();
    relinked.prev_hash = Some([1; 32]);
    assert_ne!(op_hash(&env).unwrap(), op_hash(&relinked).unwrap());
}
