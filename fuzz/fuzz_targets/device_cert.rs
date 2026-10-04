//! `DeviceCert` decode, re-encode, signature verify and identity binding.
//!
//! Scope from `docs/10-cross-cutting/testing.md` §Continuous fuzz targets:
//! "device cert decode + verify (`sunrise-crypto`)".
//!
//! A cert is the one object every peer accepts from every other peer on the
//! strength of a signature alone: `DeviceCertPublish` self-authenticates, the
//! roster inside an `identity_transition` is a list of them, and the server
//! stores them as opaque text without ever parsing one. So `from_cbor` and
//! `verify_binding` are the whole of the defence, and both run on bytes an
//! attacker chose.
//!
//! # What is asserted
//!
//! 1. **No forgery.** The corpus holds one cert, the frozen
//!    `sunrise_crypto_test_vectors::identity_transition::device_cert::ENCODED`,
//!    signed by `IDENTITY_SIGNING_SECRET`. The harness holds no other signature
//!    under that identity, so a mutation that still verifies would be a cert
//!    nobody signed. `verify` succeeding therefore demands that the signed
//!    bytes and the signature are exactly the frozen ones. Ed25519 as
//!    `ed25519-dalek` 2 checks it is strongly unforgeable (a canonical `S` and
//!    a recomputed `R`), so not even a second signature over the same body
//!    exists.
//! 2. **The binding holds.** `verify_binding` succeeding means the cert names
//!    the identity that signed it, and is refused for any other expected
//!    identity, whatever the body claims.
//! 3. **The signed bytes survive a round trip.** `from_cbor(to_cbor(c)) == c`,
//!    byte for byte in `body_bytes`. The module promises that a relayed cert
//!    still verifies after this crate re-emits it; a decoder that kept
//!    something other than the bytes that arrived would break that promise for
//!    every peer downstream.

#![no_main]

use libfuzzer_sys::fuzz_target;
use sunrise_crypto::DeviceCert;
use sunrise_crypto_test_vectors::identity_transition::{
    device_cert, IDENTITY_ID, IDENTITY_SIGNING_PUBLIC, SUCCESSOR_IDENTITY_ID,
};

fuzz_target!(|data: &[u8]| {
    let Ok(cert) = DeviceCert::from_cbor(data) else {
        return;
    };

    // 3. The re-encoding decodes to the same cert, signed bytes included.
    let reencoded = cert
        .to_cbor()
        .expect("a cert from_cbor accepted must encode");
    let again = DeviceCert::from_cbor(&reencoded)
        .expect("from_cbor rejected the encoding of a cert it had accepted");
    assert_eq!(again, cert, "to_cbor/from_cbor did not round-trip");
    assert_eq!(
        again.body_bytes(),
        cert.body_bytes(),
        "the round trip rewrote the bytes the signature covers"
    );

    // 1. Nothing but the frozen cert verifies under the frozen identity.
    if cert.verify(&IDENTITY_SIGNING_PUBLIC).is_ok() {
        assert_eq!(
            cert.body_bytes(),
            device_cert::BODY_BYTES.as_slice(),
            "a cert body nobody signed verified under the frozen identity"
        );
        assert_eq!(
            cert.sig,
            device_cert::SIG,
            "a second signature over the frozen body verified"
        );
    }

    // 2. The binding: accepted only for the identity that signed it.
    if cert
        .verify_binding(&IDENTITY_SIGNING_PUBLIC, &IDENTITY_ID)
        .is_ok()
    {
        assert_eq!(
            cert.body.identity_id, IDENTITY_ID,
            "verify_binding accepted a cert naming another identity"
        );
    }
    assert!(
        cert.verify_binding(&IDENTITY_SIGNING_PUBLIC, &SUCCESSOR_IDENTITY_ID)
            .is_err(),
        "verify_binding accepted a cert for an identity that did not sign it"
    );
});
