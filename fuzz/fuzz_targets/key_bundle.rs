//! The account key-bundle decoder, [`decode_key_bundle`], over bytes the relay
//! pins and serves to a recovering device (ADR-0069), and the
//! [`KeyBundle::verify`](sunrise_crypto::KeyBundle::verify) and open that device runs on what it decodes.
//!
//! # What is asserted
//!
//! 1. **Re-encode equality.** Whatever decodes is rebuilt, from the header the
//!    decoder returns and the input's `sealed` and `sig` fields, into exactly
//!    the input bytes. `sunrise-crypto` has no encoder for a decoded bundle
//!    (`seal_key_bundle` seals afresh, under a fresh HPKE key), and the
//!    sealed half is private, so the rebuild lays fields one to four out from
//!    [`KeyBundleHeader`] by the wire table in the module documentation of
//!    `sunrise_crypto::key_bundle` and carries fields five and six over as
//!    they were read. The decoder admits only canonical CBOR and refuses every
//!    unknown or duplicate field, so a header that dropped or altered anything
//!    the input carried fails this.
//! 2. **No forged bundle.** The harness verifies under the frozen identity,
//!    whose one device (`DEVICE_SIGNING_SECRET`) signed one bundle: the seed.
//!    So a bundle that verifies carries the seed's fields one to five, which
//!    is everything the signature covers.
//! 3. **What opens matches its manifest.** A verified bundle that opens under
//!    the frozen `ID_D_priv` yields keys whose manifest is the header's.
//!
//! The seed is `at_rest::key_bundle::BUNDLE` from
//! `sunrise-crypto-test-vectors`, byte for byte.

#![no_main]

use ciborium::value::{Integer, Value};
use libfuzzer_sys::fuzz_target;
use sunrise_cbor::{write_prefix, MagicKind, MAGIC_LEN};
use sunrise_crypto::keys::IdentityDhKeyPair;
use sunrise_crypto::{decode_key_bundle, manifest_of, KeyBundleHeader, KEY_BUNDLE_FORMAT_V};
use sunrise_crypto_test_vectors::at_rest::key_bundle as kb;
use sunrise_crypto_test_vectors::identity_transition::{
    IDENTITY_DH_SECRET, IDENTITY_ID, IDENTITY_SIGNING_PUBLIC,
};

fuzz_target!(|data: &[u8]| {
    let Ok(bundle) = decode_key_bundle(data) else {
        return;
    };

    // 1.
    let fields = map_of(data);
    let rebuilt = encode(&header_fields(bundle.header(), &fields));
    assert_eq!(rebuilt, data, "the bundle did not re-encode to its input");

    let Ok(verified) = bundle.verify(&IDENTITY_ID, &IDENTITY_SIGNING_PUBLIC) else {
        return;
    };

    // 2.
    let frozen = map_of(&kb::BUNDLE);
    for key in 1..=5 {
        assert_eq!(
            field(&fields, key),
            field(&frozen, key),
            "a bundle field {key} nobody signed verified"
        );
    }

    // 3.
    if let Ok(keys) = verified.open(&IdentityDhKeyPair::from_secret_bytes(IDENTITY_DH_SECRET)) {
        assert_eq!(
            manifest_of(&keys),
            verified.header().manifest,
            "a bundle opened to keys its manifest does not name"
        );
    }
});

fn int(n: u64) -> Value {
    Value::Integer(Integer::from(n))
}

/// The input's CBOR map, which [`decode_key_bundle`] has already accepted as
/// canonical.
fn map_of(bytes: &[u8]) -> Vec<(Value, Value)> {
    match sunrise_cbor::decode_canonical(&bytes[MAGIC_LEN..]) {
        Ok(Value::Map(entries)) => entries,
        other => panic!("an accepted bundle is not a canonical map: {other:?}"),
    }
}

fn field(entries: &[(Value, Value)], key: u64) -> &Value {
    let key = int(key);
    &entries
        .iter()
        .find(|(k, _)| *k == key)
        .expect("an accepted bundle has every field")
        .1
}

/// Fields one to four from `header`, five and six from the input.
fn header_fields(header: &KeyBundleHeader, input: &[(Value, Value)]) -> Value {
    let KeyBundleHeader {
        identity_id,
        signer_cert,
        generated_at_ms,
        manifest,
    } = header;
    let manifest = manifest
        .iter()
        .map(|e| {
            Value::Array(vec![
                Value::Bytes(e.stream_id.to_vec()),
                int(u64::from(e.epoch)),
                int(u64::from(e.keys)),
            ])
        })
        .collect();
    Value::Map(vec![
        (int(1), Value::Bytes(identity_id.to_vec())),
        (int(2), Value::Bytes(signer_cert.clone())),
        (int(3), int(*generated_at_ms)),
        (int(4), Value::Array(manifest)),
        (int(5), field(input, 5).clone()),
        (int(6), field(input, 6).clone()),
    ])
}

fn encode(map: &Value) -> Vec<u8> {
    let mut out = vec![0u8; MAGIC_LEN];
    write_prefix(&mut out, MagicKind::KeyBundle, KEY_BUNDLE_FORMAT_V);
    out.extend(sunrise_cbor::encode_canonical(map).expect("a rebuilt bundle encodes"));
    out
}
