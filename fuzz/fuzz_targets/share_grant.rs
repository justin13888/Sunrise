//! The share-grant decoder, [`decode_share_grant`], over bytes another account
//! wrote and the relay carried to a recipient (ADR-0020, ADR-0074), and the
//! [`ShareGrant::verify`](sunrise_crypto::ShareGrant::verify) and
//! [`ShareGrant::open`](sunrise_crypto::ShareGrant::open) that recipient runs
//! on what it decodes.
//!
//! # What is asserted
//!
//! 1. **Re-encode equality.** Whatever decodes re-encodes
//!    ([`ShareGrant::encode`](sunrise_crypto::ShareGrant::encode)) to exactly
//!    the input bytes. The decoder admits only canonical CBOR and refuses every
//!    unknown, repeated and missing field, so a decode that dropped or altered
//!    anything the input carried fails this.
//! 2. **No forged grant.** The harness verifies under the frozen granter
//!    (`share_grant::GRANTER_SIGNING_PUBLIC`), whose one signature is on the
//!    seed. A grant that verifies carries the seed's fields one to six, which
//!    is everything the signature covers: a mutation that still verified
//!    would be a Stream key a recipient accepted on nobody's word.
//! 3. **Open is total.** `open` under the frozen recipient returns `Ok` or an
//!    error and never panics. When it opens it yields the frozen Stream key
//!    and metadata.
//!
//! The seed is `share_grant::ENCODED` from `sunrise-crypto-test-vectors`, byte
//! for byte, at `fuzz/seeds/share_grant/frozen`.

#![no_main]

use ciborium::value::{Integer, Value};
use libfuzzer_sys::fuzz_target;
use sunrise_crypto::decode_share_grant;
use sunrise_crypto::keys::IdentityDhKeyPair;
use sunrise_crypto_test_vectors::share_grant as sg;

/// A clock before the frozen grant's expiry, so a grant that is otherwise
/// good is not refused as expired.
const NOW_MS: u64 = 1_700_000_000_000;

fuzz_target!(|data: &[u8]| {
    let Ok(grant) = decode_share_grant(data) else {
        return;
    };

    // 1.
    let encoded = grant.encode().expect("a decoded grant re-encodes");
    assert_eq!(
        encoded, data,
        "the share grant did not re-encode to its input"
    );

    // 2.
    if grant
        .verify(&sg::GRANTER_SIGNING_PUBLIC, &sg::RECIPIENT_DH_PUBLIC)
        .is_ok()
    {
        let fields = map_of(data);
        let frozen = map_of(&sg::ENCODED);
        for key in 1..=6 {
            assert_eq!(
                field(&fields, key),
                field(&frozen, key),
                "a share grant field {key} nobody signed verified"
            );
        }
    }

    // 3.
    let recipient = IdentityDhKeyPair::from_secret_bytes(sg::RECIPIENT_DH_SECRET);
    if let Ok(opened) = grant.open(
        &sg::GRANTER_SIGNING_PUBLIC,
        &sg::RECIPIENT_IDENTITY_ID,
        &recipient,
        NOW_MS,
    ) {
        assert_eq!(
            opened.key.as_bytes(),
            &sg::STREAM_KEY,
            "the frozen share grant opened to a different Stream key"
        );
        assert_eq!(opened.metadata.name, sg::NAME);
        assert_eq!(opened.metadata.color, sg::COLOR);
    }
});

/// The input's CBOR map, which [`decode_share_grant`] has already accepted as
/// canonical.
fn map_of(bytes: &[u8]) -> Vec<(Value, Value)> {
    match sunrise_cbor::decode_canonical(bytes) {
        Ok(Value::Map(entries)) => entries,
        other => panic!("an accepted share grant is not a canonical map: {other:?}"),
    }
}

fn field(entries: &[(Value, Value)], key: u64) -> &Value {
    let key = Value::Integer(Integer::from(key));
    &entries
        .iter()
        .find(|(k, _)| *k == key)
        .expect("an accepted share grant has fields one to six")
        .1
}
