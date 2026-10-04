//! Per-device chain roots and the per-stream digest (ADR-0043 §5), and the
//! retired global-order stream root.
//!
//! # Chain roots and the stream digest
//!
//! Each replica keeps, for each stream and each device, a running root over
//! that device's contiguous prefix of ops:
//!
//! ```text
//! root(d, 0) = BLAKE3::derive_key("sunrise.op_chain.init.v1", stream_id || device_id)
//! root(d, n) = BLAKE3::derive_key("sunrise.op_chain.step.v1", root(d, n-1) || op_hash(op(d, n)))
//! digest(stream, F) = BLAKE3::derive_key("sunrise.stream_digest.v1",
//!                       stream_id || for each (d, n_d, root) in F sorted by d:
//!                                      d || u64be(n_d) || root)
//! ```
//!
//! Applying a device's next op is one step, and a late op from another device
//! changes nothing already folded, which is what the global-order root below
//! could not offer. `op_hash` is [`crate::op_envelope::op_hash`].
//!
//! # The global-order stream root
//!
//! No product path calls it. ADR-0043 replaced it as the design of record
//! for detecting omission and reordering, and compaction (ADR-0059 §7) did
//! not adopt it either: a snapshot commits to its frontier's per-device chain
//! roots. It stays pinned by its frozen vectors.
//!
//! Per `docs/03-crypto/audit-and-tamper-evidence.md`:
//!
//! ```text
//! root_0    = BLAKE3("sunrise.stream_root.init.v1" || stream_id, 32)
//! root_n    = BLAKE3("sunrise.stream_root.step.v1" || root_{n-1} || env_hash_n, 32)
//! env_hash_n = BLAKE3(canonical_cbor_envelope_bytes_n, 32)
//! ```
//!
//! Concurrent ops are folded in `(hlc, device_id, seq)` order — the same key
//! entity-level LWW resolves a conflict with (ADR-0014, ADR-0016), which is
//! what makes the root and the merge agree by construction rather than by
//! coincidence.
//!
//! It was `(ts_ms_clamped, device_id_lex, seq)`, clamping the envelope's
//! timestamp into a ±5 min window around `server_first_seen_ms`. That rule had
//! no input: the relay never emitted a signed `server_first_seen_ms`
//! annotation, and the value it does return on an `Ack` is advisory — a client
//! measures its own skew with it and nothing orders ops by it. See
//! `docs/03-crypto/audit-and-tamper-evidence.md`.

use crate::blake3_kdf::derive_key_32;

const ROOT_INIT_PREFIX: &[u8] = b"sunrise.stream_root.init.v1";
const ROOT_STEP_PREFIX: &[u8] = b"sunrise.stream_root.step.v1";

/// `derive_key` context for `root(d, 0)`.
pub const CHAIN_INIT_CONTEXT: &str = "sunrise.op_chain.init.v1";
/// `derive_key` context for one chain step.
pub const CHAIN_STEP_CONTEXT: &str = "sunrise.op_chain.step.v1";
/// `derive_key` context for the stream digest.
pub const STREAM_DIGEST_CONTEXT: &str = "sunrise.stream_digest.v1";

/// `root(d, 0)`: the chain root of a device that has no op in the stream.
#[must_use]
pub fn chain_root_init(stream_id: &[u8; 16], device_id: &[u8; 16]) -> [u8; 32] {
    let mut material = [0u8; 32];
    material[..16].copy_from_slice(stream_id);
    material[16..].copy_from_slice(device_id);
    derive_key_32(CHAIN_INIT_CONTEXT, &material)
}

/// `root(d, n)` from `root(d, n - 1)` and the `op_hash` of the op at `n`.
#[must_use]
pub fn chain_root_step(prev: &[u8; 32], op_hash: &[u8; 32]) -> [u8; 32] {
    let mut material = [0u8; 64];
    material[..32].copy_from_slice(prev);
    material[32..].copy_from_slice(op_hash);
    derive_key_32(CHAIN_STEP_CONTEXT, &material)
}

/// One device's entry in a frontier: its contiguous prefix ends at `seq`, and
/// `root` is `root(device_id, seq)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrontierEntry {
    /// The device.
    pub device_id: [u8; 16],
    /// The last seq of its contiguous prefix.
    pub seq: u64,
    /// `root(device_id, seq)`.
    pub root: [u8; 32],
}

/// The stream digest at a frontier. Entries are folded in device-id order
/// whatever order they are given in, so two replicas holding the same
/// frontier compute the same digest. A frontier that names a device twice is
/// not a frontier; the caller builds it from a table keyed by device.
#[must_use]
pub fn stream_digest(stream_id: &[u8; 16], frontier: &[FrontierEntry]) -> [u8; 32] {
    let mut sorted = frontier.to_vec();
    sorted.sort_by_key(|e| e.device_id);
    let mut material = Vec::with_capacity(16 + sorted.len() * (16 + 8 + 32));
    material.extend_from_slice(stream_id);
    for e in &sorted {
        material.extend_from_slice(&e.device_id);
        material.extend_from_slice(&e.seq.to_be_bytes());
        material.extend_from_slice(&e.root);
    }
    derive_key_32(STREAM_DIGEST_CONTEXT, &material)
}

/// Initialize a per-Stream Merkle root from the stream id.
#[must_use]
pub fn stream_root_init(stream_id: &[u8; 16]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(ROOT_INIT_PREFIX);
    hasher.update(stream_id);
    *hasher.finalize().as_bytes()
}

/// Apply one envelope's hash to the root.
#[must_use]
pub fn stream_root_step(prev: &[u8; 32], env_canonical_cbor: &[u8]) -> [u8; 32] {
    let env_hash = blake3::hash(env_canonical_cbor);
    let mut hasher = blake3::Hasher::new();
    hasher.update(ROOT_STEP_PREFIX);
    hasher.update(prev);
    hasher.update(env_hash.as_bytes());
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_init() {
        let a = stream_root_init(&[7u8; 16]);
        let b = stream_root_init(&[7u8; 16]);
        assert_eq!(a, b);
    }

    #[test]
    fn step_changes_root() {
        let r0 = stream_root_init(&[1u8; 16]);
        let r1 = stream_root_step(&r0, b"some envelope bytes");
        assert_ne!(r0, r1);
    }

    #[test]
    fn order_matters() {
        let r0 = stream_root_init(&[1u8; 16]);
        let r_a_then_b = stream_root_step(&stream_root_step(&r0, b"a"), b"b");
        let r_b_then_a = stream_root_step(&stream_root_step(&r0, b"b"), b"a");
        assert_ne!(r_a_then_b, r_b_then_a);
    }
}
