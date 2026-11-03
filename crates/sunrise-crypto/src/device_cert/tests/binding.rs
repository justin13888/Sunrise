//! Issue, verify and round-trip; the signature covering the bytes that
//! arrived rather than a re-encoding of their parse; and
//! [`DeviceCert::verify_binding`] tying a cert to the identity that signed it.

use super::*;

#[test]
fn issue_verify_round_trip() {
    let mut rng = ChaCha20Rng::seed_from_u64(7);
    let identity = IdentitySigningKeyPair::generate(&mut rng);
    let device_signing = IdentitySigningKeyPair::generate(&mut rng);
    let body = fixture(device_signing.public_bytes());
    let cert = DeviceCert::issue(body, &identity).unwrap();
    cert.verify(&identity.public_bytes()).unwrap();
    // Wrong identity rejects.
    let other = IdentitySigningKeyPair::generate(&mut rng);
    assert!(matches!(
        cert.verify(&other.public_bytes()),
        Err(DeviceCertError::SigVerify)
    ));
}

#[test]
fn cbor_round_trip() {
    let mut rng = ChaCha20Rng::seed_from_u64(7);
    let identity = IdentitySigningKeyPair::generate(&mut rng);
    let body = fixture([1u8; 32]);
    let cert = DeviceCert::issue(body.clone(), &identity).unwrap();
    let bytes = cert.to_cbor().unwrap();
    let back = DeviceCert::from_cbor(&bytes).unwrap();
    assert_eq!(back, cert);
}

/// The body map of [`fixture`] with its eight keys in **descending** id
/// order.
///
/// Built from `Value::Map` directly rather than through `body_to_cbor`,
/// because the whole question is what happens to bytes this crate's encoder
/// would never emit. ciborium writes a `Value::Map`'s pairs in the order
/// given, so this is a well-formed, decodable, *non-canonical* encoding of
/// exactly the same eight fields.
fn body_cbor_with_keys_reversed() -> Vec<u8> {
    let mut fields = body_fields();
    fields.reverse();
    encode_fields(fields)
}

/// The finding: a signature must cover the bytes that arrived, not a
/// re-encoding of what they parsed to.
///
/// Two byte strings, same eight fields, different key order. They parse to
/// the same [`DeviceCertInner`] — asserted, because that is what made the
/// old code accept both — and only one of them is signed. Under the old
/// shape `verify` re-encoded the parse, so the re-ordered blob produced the
/// canonical body again and verified under a signature never taken over
/// it: one signature, two accepted blobs, and `roster_digest` hashes the
/// blob.
#[test]
fn a_body_that_was_not_the_one_signed_does_not_verify() {
    let mut rng = ChaCha20Rng::seed_from_u64(7);
    let identity = IdentitySigningKeyPair::generate(&mut rng);
    let id_s_pub = identity.public_bytes();

    let signed = DeviceCert::issue(fixture([1u8; 32]), &identity).expect("issue");
    let reordered = body_cbor_with_keys_reversed();

    // The premise: the two encodings are different bytes and the same
    // eight fields. Without this the test below would prove nothing.
    assert_ne!(
        signed.body_bytes(),
        reordered.as_slice(),
        "the reordered body must be different bytes"
    );
    assert_eq!(
        body_from_cbor(&reordered).expect("the reordered body decodes"),
        signed.body,
        "the reordered body must parse to the same fields"
    );

    // Splice the unsigned bytes in under the genuine signature.
    let mut forged = outer_cbor(reordered, 64);
    forged = replace_sig(&forged, &signed.sig);
    let parsed = DeviceCert::from_cbor(&forged).expect("it is still a well-formed cert");
    assert_eq!(parsed.body, signed.body, "and still the same fields");
    assert!(
        matches!(parsed.verify(&id_s_pub), Err(DeviceCertError::SigVerify)),
        "a body the identity never signed must not verify"
    );
}

/// `{1: body_bytes, 2: sig}` with the signature replaced.
fn replace_sig(outer: &[u8], sig: &[u8; 64]) -> Vec<u8> {
    let value: Value = ciborium::de::from_reader(outer).expect("outer decodes");
    let Value::Map(map) = value else {
        panic!("the outer cert is a map")
    };
    let rebuilt: Vec<(Value, Value)> = map
        .into_iter()
        .map(|(k, v)| {
            if k == Value::Integer(Integer::from(2)) {
                (k, Value::Bytes(sig.to_vec()))
            } else {
                (k, v)
            }
        })
        .collect();
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(rebuilt), &mut out).expect("outer re-encodes");
    out
}

/// `to_cbor` writes the body verbatim, so a cert that arrived from
/// elsewhere survives a round trip through this crate byte for byte and
/// still verifies. Without this, relaying a cert would re-canonicalize it
/// and break a signature the relayer had just checked.
#[test]
fn a_round_trip_preserves_the_body_bytes_exactly() {
    let mut rng = ChaCha20Rng::seed_from_u64(11);
    let identity = IdentitySigningKeyPair::generate(&mut rng);
    let cert = DeviceCert::issue(fixture([1u8; 32]), &identity).expect("issue");
    let once = DeviceCert::from_cbor(&cert.to_cbor().unwrap()).unwrap();
    let twice = DeviceCert::from_cbor(&once.to_cbor().unwrap()).unwrap();
    assert_eq!(once.body_bytes(), cert.body_bytes());
    assert_eq!(twice.body_bytes(), cert.body_bytes());
    twice
        .verify(&identity.public_bytes())
        .expect("it still verifies after two round trips");
}

/// A tail inside the body byte string is signed junk, not ignored junk:
/// the outer `bstr` fixes its length, so `ciborium` would read the map and
/// stop. Refused, for the same reason the outer map refuses a tail.
#[test]
fn a_body_with_trailing_bytes_is_refused() {
    let mut body = body_to_cbor(&fixture([1u8; 32])).expect("the fixture encodes");
    body.extend_from_slice(b"\x00tail");
    assert!(matches!(
        DeviceCert::from_cbor(&outer_cbor(body, 64)),
        Err(DeviceCertError::Cbor(_))
    ));
}

/// The body must be a byte string. A nested map is the *old* shape, and a
/// build that silently accepted it would have re-introduced the
/// re-encoding path.
#[test]
fn a_body_that_is_not_a_byte_string_is_refused() {
    let encoded = body_to_cbor(&fixture([1u8; 32])).expect("the fixture encodes");
    let nested: Value =
        ciborium::de::from_reader(encoded.as_slice()).expect("its own output decodes");
    let map = vec![
        (Value::Integer(Integer::from(1)), nested),
        (
            Value::Integer(Integer::from(2)),
            Value::Bytes(vec![0xab; 64]),
        ),
    ];
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(map), &mut out).expect("outer map encodes");
    assert!(matches!(
        DeviceCert::from_cbor(&out),
        Err(DeviceCertError::BadField("device cert outer"))
    ));
}

/// A cert can verify and still be a lie about which identity it belongs
/// to: `identity_id` is a signed *field*, and only recomputing it from the
/// signing key catches a rewrite.
#[test]
fn verify_binding_catches_a_rewritten_identity_id() {
    let mut rng = ChaCha20Rng::seed_from_u64(7);
    let identity = IdentitySigningKeyPair::generate(&mut rng);
    let id_s_pub = identity.public_bytes();
    let real_id = crate::identity::identity_id_from_pub(&id_s_pub);

    let mut body = fixture([1u8; 32]);
    body.identity_id = real_id;
    let good = DeviceCert::issue(body.clone(), &identity).unwrap();
    good.verify_binding(&id_s_pub, &real_id).unwrap();

    // Same signer, a body claiming somebody else's identity.
    body.identity_id = [0xaa; 16];
    let liar = DeviceCert::issue(body, &identity).unwrap();
    liar.verify(&id_s_pub).expect("the signature is genuine");
    assert!(matches!(
        liar.verify_binding(&id_s_pub, &real_id),
        Err(DeviceCertError::IdentityMismatch)
    ));
}

/// A well-formed cert from a *different* identity is refused even though it
/// verifies under its own key: the vault names the identity it trusts.
#[test]
fn verify_binding_refuses_another_identity() {
    let mut rng = ChaCha20Rng::seed_from_u64(7);
    let ours = IdentitySigningKeyPair::generate(&mut rng);
    let theirs = IdentitySigningKeyPair::generate(&mut rng);
    let our_id = crate::identity::identity_id_from_pub(&ours.public_bytes());
    let their_pub = theirs.public_bytes();

    let mut body = fixture([1u8; 32]);
    body.identity_id = crate::identity::identity_id_from_pub(&their_pub);
    let cert = DeviceCert::issue(body, &theirs).unwrap();
    cert.verify_binding(&their_pub, &body_identity(&cert))
        .unwrap();
    assert!(matches!(
        cert.verify_binding(&their_pub, &our_id),
        Err(DeviceCertError::IdentityMismatch)
    ));
}

fn body_identity(cert: &DeviceCert) -> [u8; 16] {
    cert.body.identity_id
}
