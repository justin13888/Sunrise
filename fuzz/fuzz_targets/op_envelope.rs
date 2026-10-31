//! `OpEnvelope` decode, canonical re-encode, signature verify, AEAD open.
//!
//! Scope from `docs/10-cross-cutting/testing.md` §Continuous fuzz targets:
//! "`sunrise-crypto` envelope decode + signature verify path".
//!
//! `crates/sunrise-crypto/tests/op_envelope_proptest.rs` already generates
//! *valid* envelopes and checks what the codec does with them. This generates
//! arbitrary bytes, which is the half a property test cannot reach: the
//! decoder's own failure modes — a truncated magic prefix, a CBOR map with a
//! duplicate or out-of-order key, a byte string of the wrong length in a fixed
//! field, an unknown field id the container must preserve verbatim.
//!
//! # The assertion, and why it is the right one
//!
//! Decoding must not panic — that much is free with any target. What is worth
//! asserting is that **signing and verifying agree about what the canonical
//! bytes of an envelope are**. Both go through the same private `encode_cbor`,
//! but through *different* omission masks, and they meet only in the
//! `sig_input_bytes` hash. An envelope carrying fuzzer-chosen `unknown` fields
//! is exactly the input that would separate them: the unknown map is merged
//! into the field-id ordering at encode time, and a bug there shows up as a
//! signature this process produced and cannot check.
//!
//! The relay reads the same bytes through a second decoder,
//! `sunrise_cbor::decode_envelope_header`, and routes on what it returns. So an
//! envelope the client decoder accepts must also pass the relay's, with the
//! same `(stream_id, device_id, seq)`. The converse does not hold: the relay
//! reads only the routing fields, so it accepts bytes the client refuses for a
//! fault in any other field.
//!
//! The seed corpus is the two frozen vectors in `sunrise-crypto-test-vectors`
//! — one signed-only envelope, one AEAD-sealed — so the fuzzer starts past the
//! six-byte magic prefix and the CBOR map header instead of spending its budget
//! rediscovering them.

#![no_main]

use libfuzzer_sys::fuzz_target;
use sunrise_cbor::decode_envelope_header;
use sunrise_crypto::{
    decode_envelope, open_envelope_unverified, sign_envelope, verify_envelope,
    DeviceSigningKeyPair, StreamKey,
};

fuzz_target!(|data: &[u8]| {
    // Before the early return, so the relay's decoder sees the bytes the client
    // decoder refuses as well: it reads them first, from an unauthenticated peer.
    let header = decode_envelope_header(data);

    let Ok(env) = decode_envelope(data) else {
        return;
    };

    let header = header.expect("the relay refused an envelope the client decoder accepts");
    assert_eq!(
        (header.stream_id, header.device_id, header.seq),
        (env.stream_id, env.device_id, env.seq),
        "the relay and the client decoder read different routing fields"
    );

    // The device key the frozen vectors were signed with. Against a mutated
    // envelope this almost always fails, and that is the point: `verify` must
    // return `SigVerify`, not panic, on a signature over bytes it can still
    // canonicalise.
    let key = DeviceSigningKeyPair::from_secret_bytes(
        &sunrise_crypto_test_vectors::DEVICE_SIGNING_SECRET,
    );
    let _ = verify_envelope(&env, &sunrise_crypto_test_vectors::DEVICE_SIGNING_PUBLIC);

    // The real invariant. Re-sign the decoded envelope with a key we hold, then
    // verify it with that key's public half. These are the two directions of
    // one canonicalisation; if they disagree the envelope decoded into a shape
    // the encoder cannot reproduce, which is a wire-format bug whatever the
    // input was.
    let mut resigned = env.clone();
    if sign_envelope(&mut resigned, &key).is_ok() {
        assert!(
            verify_envelope(&resigned, &key.public_bytes()).is_ok(),
            "sign_envelope produced a signature verify_envelope rejects"
        );
    }

    // Signing must not have disturbed anything but the signature: the decoded
    // envelope is what a peer would re-emit, and a decoder that lets `sign`
    // rewrite a field has silently changed the message.
    resigned.sig = env.sig;
    assert_eq!(
        resigned, env,
        "sign_envelope changed a field other than `sig`"
    );

    // The AEAD path, under a fixed stream key. Every outcome here is an `Err`
    // for a mutated envelope; what is being fuzzed is the AAD reconstruction
    // that runs before the tag check.
    let stream_key = StreamKey::from_bytes([0x44; 32]);
    let _ = open_envelope_unverified(&env, &stream_key);
});
