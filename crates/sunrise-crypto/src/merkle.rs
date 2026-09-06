//! Per-Stream Merkle root for tamper detection.
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

const ROOT_INIT_PREFIX: &[u8] = b"sunrise.stream_root.init.v1";
const ROOT_STEP_PREFIX: &[u8] = b"sunrise.stream_root.step.v1";

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
