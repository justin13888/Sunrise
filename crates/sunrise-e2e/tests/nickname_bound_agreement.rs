//! The device nickname bound is one number enforced in two crates that do not
//! depend on each other. This is what makes them agree (issue #102).
//!
//! `sunrise-crypto` refuses a `DeviceCert` whose nickname exceeds
//! `MAX_NICKNAME_BYTES`, in the encoder and again in the decoder.
//! `sunrise-server` refuses a `POST /api/v1/devices` whose `nickname` exceeds
//! its own constant of the same name. Neither crate can see the other: the
//! relay never parses a cert — it stores it as opaque text — so there is no
//! dependency edge to hang a shared constant on, and adding one to carry a
//! single integer would make the server build the whole crypto crate for a
//! length check.
//!
//! `sunrise-e2e` already depends on both, so the agreement is asserted here.
//! The failure it prevents is not hypothetical: a server bound *below* the
//! crypto one leaves the pairing path minting certs that `POST /devices`
//! refuses — the user sees a device that pairs locally and then cannot
//! register, quoting a limit their nickname appears to satisfy. A server bound
//! *above* it registers a device whose cert no other device's decoder will
//! read.

#![allow(clippy::missing_panics_doc)]

use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use sunrise_crypto::keys::IdentitySigningKeyPair;
use sunrise_crypto::{DeviceCert, DeviceCertError, DeviceCertInner};

/// The bound the relay enforces on a registration request.
const SERVER_BOUND: usize = sunrise_server::api::devices::MAX_NICKNAME_BYTES;
/// The bound the cert codec enforces on the same field.
const CRYPTO_BOUND: usize = sunrise_crypto::MAX_NICKNAME_BYTES;

/// The direct statement: one number, two crates.
#[test]
fn the_two_nickname_bounds_are_the_same_number() {
    assert_eq!(
        CRYPTO_BOUND, SERVER_BOUND,
        "sunrise-crypto and sunrise-server must agree on the nickname bound; \
         a divergence mints certs the relay refuses, or registers devices whose \
         certs no peer can decode"
    );
}

fn cert_with_nickname(nickname: String) -> Result<DeviceCert, DeviceCertError> {
    let mut rng = ChaCha20Rng::seed_from_u64(102);
    let identity = IdentitySigningKeyPair::generate(&mut rng);
    let device = IdentitySigningKeyPair::generate(&mut rng);
    DeviceCert::issue(
        DeviceCertInner {
            v: 1,
            device_id: [7u8; 16],
            d_s_pub: device.public_bytes(),
            d_d_pub: [9u8; 32],
            identity_id: [3u8; 16],
            created_at_ms: 1_700_000_000_000,
            nickname,
            platform: "macos15".to_string(),
        },
        &identity,
    )
    .and_then(|c| c.to_cbor().map(|_| c))
}

/// The behavioural half, and the one that survives somebody deleting the
/// equality assert above: the cert codec's accept/refuse boundary is read off
/// the **server's** constant. Lower the server's number and the `+ 1` case
/// starts encoding; raise it and the exact case stops.
#[test]
fn the_cert_codec_admits_exactly_the_nickname_the_relay_admits() {
    let at_bound = "n".repeat(SERVER_BOUND);
    let cert = cert_with_nickname(at_bound.clone()).expect("a nickname at the relay's bound");
    let round_tripped = DeviceCert::from_cbor(&cert.to_cbor().unwrap()).expect("it decodes again");
    assert_eq!(round_tripped.body.nickname, at_bound);

    let over_bound = "n".repeat(SERVER_BOUND + 1);
    assert!(
        matches!(
            cert_with_nickname(over_bound),
            Err(DeviceCertError::Validation("nickname length"))
        ),
        "one byte past the relay's bound must not encode"
    );
}
