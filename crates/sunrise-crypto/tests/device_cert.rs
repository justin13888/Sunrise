//! Identity-signed device certs, from the outside.
//!
//! Before ADR-0024 a `DeviceCert` was signed by the device's own key and its
//! `identity_id` was derived from that same key, so "verified" meant nothing
//! more than "internally consistent" — every self-signed cert was valid, and
//! trust rested entirely on whatever channel delivered it. These tests pin the
//! two properties that replace it: a cert is only valid under the identity that
//! issued it, and the identity it *claims* must be the identity that signed it.

use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use sunrise_crypto::keys::{DeviceDhKeyPair, DeviceSigningKeyPair, IdentitySigningKeyPair};
use sunrise_crypto::{identity_id_from_pub, DeviceCert, DeviceCertError, DeviceCertInner};

fn body(
    device_signing: &DeviceSigningKeyPair,
    device_dh: &DeviceDhKeyPair,
    identity_id: [u8; 16],
) -> DeviceCertInner {
    DeviceCertInner {
        v: 1,
        device_id: [0x42; 16],
        d_s_pub: device_signing.public_bytes(),
        d_d_pub: device_dh.public_bytes(),
        identity_id,
        created_at_ms: 1_700_000_000_000,
        nickname: "a laptop".to_string(),
        platform: "macos".to_string(),
    }
}

#[test]
fn a_cert_issued_by_one_identity_does_not_verify_under_another() {
    let mut rng = ChaCha20Rng::seed_from_u64(1);
    let a = IdentitySigningKeyPair::generate(&mut rng);
    let b = IdentitySigningKeyPair::generate(&mut rng);
    let d_s = DeviceSigningKeyPair::generate(&mut rng);
    let d_d = DeviceDhKeyPair::generate(&mut rng);

    let a_id = identity_id_from_pub(&a.public_bytes());
    let cert = DeviceCert::issue(body(&d_s, &d_d, a_id), &a).unwrap();

    cert.verify(&a.public_bytes()).expect("issuer verifies");
    assert!(matches!(
        cert.verify(&b.public_bytes()),
        Err(DeviceCertError::SigVerify)
    ));
    assert!(matches!(
        cert.verify_binding(&b.public_bytes(), &identity_id_from_pub(&b.public_bytes())),
        Err(DeviceCertError::SigVerify)
    ));
}

/// The self-signature ADR-0024 removes: a device's own key is not the identity,
/// and a cert signed by it is refused rather than trusted.
#[test]
fn a_self_signed_cert_is_not_an_identity_signed_one() {
    let mut rng = ChaCha20Rng::seed_from_u64(2);
    let identity = IdentitySigningKeyPair::generate(&mut rng);
    let d_s = DeviceSigningKeyPair::generate(&mut rng);
    let d_d = DeviceDhKeyPair::generate(&mut rng);
    let identity_id = identity_id_from_pub(&identity.public_bytes());

    // A device signing its own cert. `DeviceCert::issue` takes an
    // `IdentitySigningKeyPair`, so this needs a deliberate reconstruction from
    // the device seed — which is exactly the step the old alias made invisible.
    let impostor = IdentitySigningKeyPair::from_secret_bytes(&d_s.secret_bytes());
    let cert = DeviceCert::issue(body(&d_s, &d_d, identity_id), &impostor).unwrap();

    assert!(matches!(
        cert.verify(&identity.public_bytes()),
        Err(DeviceCertError::SigVerify)
    ));
}

/// `identity_id` is a signed field, so a signature check alone does not stop a
/// holder of `ID_S_priv` claiming somebody else's identity. `verify_binding`
/// recomputes it.
#[test]
fn verify_passes_where_verify_binding_refuses_a_rewritten_identity() {
    let mut rng = ChaCha20Rng::seed_from_u64(3);
    let identity = IdentitySigningKeyPair::generate(&mut rng);
    let d_s = DeviceSigningKeyPair::generate(&mut rng);
    let d_d = DeviceDhKeyPair::generate(&mut rng);
    let real = identity_id_from_pub(&identity.public_bytes());

    let mut b = body(&d_s, &d_d, real);
    b.identity_id = [0xff; 16];
    let cert = DeviceCert::issue(b, &identity).unwrap();

    cert.verify(&identity.public_bytes())
        .expect("the signature is genuine — that is the point");
    assert!(matches!(
        cert.verify_binding(&identity.public_bytes(), &real),
        Err(DeviceCertError::IdentityMismatch)
    ));
}

/// A cert that is entirely self-consistent under *its own* identity is still
/// refused by a vault that belongs to a different identity.
#[test]
fn a_valid_cert_from_a_foreign_identity_is_refused() {
    let mut rng = ChaCha20Rng::seed_from_u64(4);
    let ours = IdentitySigningKeyPair::generate(&mut rng);
    let theirs = IdentitySigningKeyPair::generate(&mut rng);
    let d_s = DeviceSigningKeyPair::generate(&mut rng);
    let d_d = DeviceDhKeyPair::generate(&mut rng);

    let their_id = identity_id_from_pub(&theirs.public_bytes());
    let cert = DeviceCert::issue(body(&d_s, &d_d, their_id), &theirs).unwrap();

    cert.verify_binding(&theirs.public_bytes(), &their_id)
        .expect("valid within its own identity");
    assert!(matches!(
        cert.verify_binding(
            &theirs.public_bytes(),
            &identity_id_from_pub(&ours.public_bytes())
        ),
        Err(DeviceCertError::IdentityMismatch)
    ));
}

/// The cert survives a CBOR round-trip with its signature intact — a re-encode
/// that moved a byte would invalidate every stored cert on the next open.
#[test]
fn cbor_round_trip_preserves_verifiability() {
    let mut rng = ChaCha20Rng::seed_from_u64(5);
    let identity = IdentitySigningKeyPair::generate(&mut rng);
    let d_s = DeviceSigningKeyPair::generate(&mut rng);
    let d_d = DeviceDhKeyPair::generate(&mut rng);
    let id = identity_id_from_pub(&identity.public_bytes());

    let cert = DeviceCert::issue(body(&d_s, &d_d, id), &identity).unwrap();
    let bytes = cert.to_cbor().unwrap();
    let back = DeviceCert::from_cbor(&bytes).unwrap();
    assert_eq!(back, cert);
    back.verify_binding(&identity.public_bytes(), &id).unwrap();
    assert_eq!(back.to_cbor().unwrap(), bytes);
}
