//! Pairing rate limits + email-hash helper.
//!
//! Per `docs/03-crypto/pairing-and-onboarding.md`.

const EMAIL_HASH_DOMAIN: &str = "sunrise.account_email_hash.v1";

/// Pair attempts per rolling hour per `account_email_hash`.
pub const RATE_LIMIT_HOURLY: u32 = 10;

/// Pair attempts per rolling day per `account_email_hash`.
pub const RATE_LIMIT_DAILY: u32 = 30;

/// Compute the 4-byte (8 hex chars) `account_email_hash`. Email is normalized
/// (lowercase + trim) before hashing. The hash is non-reversible and used
/// only as the rate-limit bucket key — never to identify a user across
/// accounts.
#[must_use]
pub fn account_email_hash(email: &str) -> [u8; 4] {
    let normalized = email.trim().to_ascii_lowercase();
    let mut hasher = blake3::Hasher::new_derive_key(EMAIL_HASH_DOMAIN);
    hasher.update(normalized.as_bytes());
    let mut out = [0u8; 4];
    hasher.finalize_xof().fill(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_normalized() {
        let a = account_email_hash("Justin@Example.com");
        let b = account_email_hash(" justin@example.com  ");
        assert_eq!(a, b);
    }

    #[test]
    fn different_emails_diverge() {
        let a = account_email_hash("a@example.com");
        let b = account_email_hash("b@example.com");
        assert_ne!(a, b);
    }
}
