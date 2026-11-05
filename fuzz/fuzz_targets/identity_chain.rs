//! The identity-chain record decoder, [`IdentityChain::decode`], over bytes
//! the relay serves beside the recovery blob (ADR-0070), and the
//! [`IdentityChain::verify`] and [`IdentityChain::carry_walk`] a recovering
//! vault runs on what it decodes.
//!
//! # What is asserted
//!
//! 1. **Re-encode equality.** Whatever decodes encodes back to exactly the
//!    input bytes, and that encoding decodes to the same chain. The decoder
//!    admits only canonical CBOR and refuses every unknown or duplicate field,
//!    and a link's predecessor is positional rather than carried, so there is
//!    nothing it may skip or normalise: one record has one encoding.
//! 2. **No forged link.** The harness's frozen identity
//!    (`IDENTITY_SIGNING_PUBLIC`) signed one transition and its successor
//!    accepted only that one, so a chain that verifies from the frozen genesis
//!    is the genesis alone or the genesis and that one transition, body and
//!    signatures exactly. Anything else verifying is a rotation the account
//!    never made. The link's `meta_epoch`, HLC, emitter and carry share are
//!    unsigned (the module documentation says why), so they are not compared.
//! 3. **The carry walk stays on the chain.** For a verified chain, walking
//!    the carry shares from the genesis with the frozen `ID_D_priv` either
//!    reaches the last link or names a link the chain has.
//!
//! The seeds are the genesis alone, and the genesis with the frozen
//! transition both carrying the code forward (a share sealed to the frozen
//! `ID_D_pub`) and retiring it.

#![no_main]

use libfuzzer_sys::fuzz_target;
use sunrise_crypto::{CarryWalk, IdentityChain, IdentityTransitionBody};
use sunrise_crypto_test_vectors::identity_transition::{
    transition, IDENTITY_DH_SECRET, IDENTITY_ID,
};

fuzz_target!(|data: &[u8]| {
    let Ok(chain) = IdentityChain::decode(data) else {
        return;
    };

    // 1.
    let bytes = chain.encode().expect("a decoded chain encodes");
    assert_eq!(bytes, data, "the chain did not re-encode to its input");
    let back = IdentityChain::decode(&bytes).expect("an encoded chain decodes");
    assert_eq!(back, chain, "the chain changed in a round trip");

    let Ok(()) = chain.verify(&IDENTITY_ID) else {
        return;
    };

    // 2.
    if let Some(link) = chain.links.first() {
        let frozen: IdentityTransitionBody =
            sunrise_cbor::decode_canonical(&transition::BODY_CBOR).expect("the frozen body");
        assert_eq!(chain.links.len(), 1, "a second link verified");
        assert_eq!(link.body, frozen, "a link body nobody signed verified");
        assert_eq!(link.sigs.prev_sig, transition::PREV_SIG);
        assert_eq!(link.sigs.next_sig, transition::NEXT_SIG);
    }

    // 3.
    match chain.carry_walk(&IDENTITY_ID, &IDENTITY_DH_SECRET) {
        CarryWalk::Reaches { links } => assert_eq!(links, chain.links.len()),
        CarryWalk::Retired { at_link } => assert!(at_link < chain.links.len()),
        CarryWalk::NotOnChain => panic!("the genesis is not on its own chain"),
    }
});
