//! Recovery-blob decode: magic prefix, length floor, Argon2id parameters, AEAD
//! tag, and the inner CBOR payload.
//!
//! Scope from `docs/10-cross-cutting/testing.md` §Continuous fuzz targets:
//! "recovery-blob decode + KDF input validation".
//!
//! # Throughput, stated up front
//!
//! `unseal_recovery_blob` runs Argon2id at m=64 MiB, t=3 for every input that
//! clears the magic prefix and the length floor. That is ~100 ms of work per
//! iteration by design — it is the parameter set that makes a stolen blob
//! expensive to grind — so this target runs at single-digit executions per
//! second once the corpus has taught the fuzzer to keep the six-byte prefix.
//! It is a nightly-budget target, not a per-commit one, and
//! `docs/10-cross-cutting/testing.md` says so where it lists the targets.
//!
//! The cheap half is still worth fuzzing at speed: everything before the KDF —
//! `decode_prefix`, the `MAGIC_LEN + SALT + NONCE + TAG` floor, the two
//! fixed-width slices — runs on every input including the ones the fuzzer
//! generates at random, and those cost nothing.
//!
//! # The assertion
//!
//! Failure is the expected answer: the corpus holds one blob sealed under
//! `SEED`, and a mutation of it either authenticates (it is the seed itself)
//! or does not. What must never happen is a payload coming back whose
//! `identity_id` is not the one demanded, because the caller uses that field
//! as the account it just recovered — the AAD binding and the explicit
//! re-check in `unseal_recovery_blob` are both there to prevent exactly that,
//! and this asserts the two of them together.

#![no_main]

use libfuzzer_sys::fuzz_target;
use sunrise_crypto::unseal_recovery_blob;

/// The BIP-39-derived seed the corpus blob was sealed under. A fixed value,
/// not a fuzzer-chosen one: the KDF is the expensive step, and letting the
/// fuzzer move the seed would mean every iteration derives a key for a blob it
/// has no chance of opening.
const SEED: [u8; 32] = [0x9a; 32];

/// The identity the corpus blob belongs to.
const IDENTITY_ID: [u8; 16] = [0x5c; 16];

fuzz_target!(|data: &[u8]| {
    let Ok(payload) = unseal_recovery_blob(data, &SEED, &IDENTITY_ID) else {
        return;
    };
    assert_eq!(
        payload.identity_id, IDENTITY_ID,
        "unseal_recovery_blob returned a payload bound to a different identity"
    );
});
