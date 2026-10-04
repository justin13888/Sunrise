//! `identity_transition` verification: the signed hand-over from one account
//! identity to its successor (ADR-0037).
//!
//! Scope from `docs/10-cross-cutting/testing.md` §Continuous fuzz targets:
//! "identity-transition chain verify (`sunrise-crypto`)".
//!
//! # Input encoding
//!
//! `canonical_cbor(IdentityTransitionBody) || prev_sig(64) || next_sig(64)`:
//! the last 128 bytes are the signature pair and everything before them is the
//! body, decoded with `sunrise_cbor::decode_canonical`, the decoder whose
//! encoder `body_hash` hashes. The one seed is the three frozen literals in
//! `sunrise_crypto_test_vectors::identity_transition::transition`
//! concatenated, so the fuzzer starts from a transition that verifies.
//!
//! # What is asserted
//!
//! The harness holds one transition, signed by `IDENTITY_SIGNING_SECRET` and
//! accepted by `SUCCESSOR_SIGNING_SECRET`, and no other signature under either
//! key. So:
//!
//! 1. **No forgery of the hand-over.** `verify_identity_transition` against the
//!    frozen outgoing identity succeeds only for the frozen body and the frozen
//!    pair. Anything else would be a rotation the account never authorized —
//!    the attack that moves every surviving device onto a key the attacker
//!    holds.
//! 2. **No forgery of the acceptance.** `verify_successor_signature` alone, the
//!    check the apply path runs before it knows the predecessor, verifies
//!    `next_sig` under the body's *own* `to_id_s_pub`. So it accepts any body
//!    correctly signed by the successor key that body names, and the fuzzer may
//!    name a key it holds; what the check guarantees is only that whoever
//!    authored the payload held the named successor's key. The harness
//!    therefore asserts it for bodies naming the frozen successor
//!    (`SUCCESSOR_SIGNING_PUBLIC`), whose key signed nothing but the frozen
//!    transition: there it succeeds only for the frozen body *and* the frozen
//!    `prev_sig`, because `next_sig` covers both, which is what stops a
//!    successor signature being lifted onto another predecessor's transition.
//! 3. **The two checks agree.** Full verification implies the successor check;
//!    a body the chain fold believes must be one the apply path recorded.
//! 4. **The two signatures cannot stand in for each other.** The pair swapped
//!    never verifies: the domain strings differ so that one signature is never
//!    readable as the other.

#![no_main]

use libfuzzer_sys::fuzz_target;
use sunrise_crypto::{
    verify_identity_transition, verify_successor_signature, IdentityTransitionBody,
    IdentityTransitionSigs,
};
use sunrise_crypto_test_vectors::identity_transition::{
    transition, IDENTITY_SIGNING_PUBLIC, SUCCESSOR_SIGNING_PUBLIC,
};

const SIGS_LEN: usize = 128;

fuzz_target!(|data: &[u8]| {
    let Some(split) = data.len().checked_sub(SIGS_LEN) else {
        return;
    };
    let (body_bytes, sig_bytes) = data.split_at(split);
    let Ok(body) = sunrise_cbor::decode_canonical::<IdentityTransitionBody>(body_bytes) else {
        return;
    };
    let mut prev_sig = [0u8; 64];
    let mut next_sig = [0u8; 64];
    prev_sig.copy_from_slice(&sig_bytes[..64]);
    next_sig.copy_from_slice(&sig_bytes[64..]);
    let sigs = IdentityTransitionSigs { prev_sig, next_sig };

    // `decode_canonical` admits only bytes the encoder would write, so the body
    // that verifies is the body that was hashed.
    let is_frozen_body = body_bytes == transition::BODY_CBOR.as_slice();

    let full = verify_identity_transition(&body, &IDENTITY_SIGNING_PUBLIC, &sigs);
    let successor = verify_successor_signature(&body, &sigs);

    // 1.
    if full.is_ok() {
        assert!(
            is_frozen_body && prev_sig == transition::PREV_SIG && next_sig == transition::NEXT_SIG,
            "a transition nobody signed verified under the frozen outgoing identity"
        );
    }
    // 2. Only a body naming the frozen successor: under a key the input names
    // itself, a correct `next_sig` is the input's own signature, not a forgery.
    if successor.is_ok() && body.to_id_s_pub == SUCCESSOR_SIGNING_PUBLIC {
        assert!(
            is_frozen_body && prev_sig == transition::PREV_SIG && next_sig == transition::NEXT_SIG,
            "a successor signature verified over a body or prev_sig it does not cover"
        );
    }
    // 3.
    if full.is_ok() {
        assert!(
            successor.is_ok(),
            "verify_identity_transition accepted what verify_successor_signature refuses"
        );
    }
    // 4.
    let swapped = IdentityTransitionSigs {
        prev_sig: next_sig,
        next_sig: prev_sig,
    };
    assert!(
        verify_identity_transition(&body, &IDENTITY_SIGNING_PUBLIC, &swapped).is_err(),
        "the signature pair verified with prev_sig and next_sig swapped"
    );
});
