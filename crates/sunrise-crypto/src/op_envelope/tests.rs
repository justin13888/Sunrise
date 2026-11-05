//! `OpEnvelope`'s unit tests, one file per theme, sharing the fixtures below.
//!
//! * [`round_trip`]: seal, decode, verify and open, and the gates that refuse
//!   a tampered or foreign envelope.
//! * [`versions`]: the container format (field 1, the prefix floor) and the
//!   document schema (field 12), kept apart.
//! * [`field13`]: the document schema's fingerprint.
//! * [`chain`]: the chain fields 14 and 15, and [`super::op_hash`].

use super::*;
use crate::keys::DeviceSigningKeyPair;
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;

mod chain;
mod field13;
mod round_trip;
mod versions;

fn fixed_signing() -> DeviceSigningKeyPair {
    let mut rng = ChaCha20Rng::seed_from_u64(7);
    DeviceSigningKeyPair::generate(&mut rng)
}

fn fixed_stream_key() -> StreamKey {
    StreamKey::from_bytes([1u8; 32])
}

/// Build an envelope carrying an arbitrary `doc_schema_v`, the way a
/// future build would.
fn envelope_at_doc_schema(doc_schema_v: u32, signing: &DeviceSigningKeyPair) -> Vec<u8> {
    seal_envelope(
        OpEnvelope {
            v: u32::from(ENVELOPE_FORMAT_V),
            doc_schema_v,
            schema_fp: None,
            stream_id: [2u8; 16],
            device_id: [3u8; 16],
            seq: 1,
            hlc: Hlc::at(1_700_000_000_000),
            aead_alg: AeadAlgId::None,
            sig_alg: SigAlgId::Ed25519,
            epoch: 0,
            nonce: [0u8; AEAD_NONCE_LEN],
            payload: b"inner".to_vec(),
            sig: [0u8; 64],
            prev_hash: None,
            heads: Vec::new(),
            unknown: BTreeMap::new(),
        },
        None,
        signing,
    )
    .unwrap()
}

fn contains_subseq(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}
