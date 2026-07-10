//! Short Authentication String — 6-digit decimal SAS code.
//!
//! Per `docs/03-crypto/pairing-and-onboarding.md`:
//!
//! ```text
//! sas_int = decimal_be(BLAKE3("sunrise.pair_sas.v1" || handshake_hash, 3)) mod 1_000_000
//! sas     = format!("{:06}", sas_int)
//! ```

const SAS_DOMAIN: &str = "sunrise.pair_sas.v1";

/// Length of the formatted SAS string (always 6 digits).
pub const SAS_LEN: usize = 6;

/// Compute the 6-digit SAS from a Noise XX handshake hash (32 bytes).
#[must_use]
pub fn compute_sas(handshake_hash: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new_derive_key(SAS_DOMAIN);
    hasher.update(handshake_hash);
    let mut out = [0u8; 3];
    hasher.finalize_xof().fill(&mut out);
    let n: u32 = (u32::from(out[0]) << 16) | (u32::from(out[1]) << 8) | u32::from(out[2]);
    let modded = n % 1_000_000;
    format!("{modded:06}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_six_digits() {
        let s = compute_sas(&[7u8; 32]);
        assert_eq!(s.len(), SAS_LEN);
        assert!(s.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn deterministic() {
        assert_eq!(compute_sas(&[7u8; 32]), compute_sas(&[7u8; 32]));
    }

    #[test]
    fn distinct_inputs_diverge() {
        let a = compute_sas(&[1u8; 32]);
        let b = compute_sas(&[2u8; 32]);
        assert_ne!(a, b);
    }
}
