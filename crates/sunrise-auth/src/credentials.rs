//! What a completed login yields, and when it needs renewing.

use serde::{Deserialize, Serialize};

/// Renew at 75% of the token's lifetime.
///
/// `docs/06-server/auth.md` fixes this number. It is a deliberate compromise:
/// early enough that a renewal has three failed attempts' worth of runway
/// before the credential actually dies, late enough that a normal session does
/// not spend its time talking to the issuer.
pub const RENEW_AT_FRACTION: u64 = 75;

/// A bearer, its renewal handle, and its deadline.
///
/// `Serialize`/`Deserialize` because this is what a client persists between
/// runs — see the note on `Debug` below before writing it anywhere world- or
/// group-readable.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Credentials {
    /// The bearer presented to the relay.
    pub access_token: String,
    /// The refresh token, when the issuer granted one.
    ///
    /// Absent is legal and not an error: an issuer that does not grant one
    /// simply means renewal requires the browser again.
    pub refresh_token: Option<String>,
    /// Wall-clock ms at which `access_token` expires.
    pub expires_at_ms: u64,
    /// Wall-clock ms at which a well-behaved client should renew — 75% of the
    /// way through the lifetime, per [`RENEW_AT_FRACTION`].
    pub renew_at_ms: u64,
}

// Hand-written. These are live credentials and this type is the natural thing
// to include in an error context or a debug dump.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at_ms", &self.expires_at_ms)
            .field("renew_at_ms", &self.renew_at_ms)
            .finish()
    }
}

impl Credentials {
    /// Build from an issuer's `expires_in` (seconds) against `now_ms`.
    ///
    /// `expires_in` is optional in OAuth 2.0. An issuer that omits it is
    /// treated as **already due for renewal** rather than as never expiring:
    /// the relay enforces the token's real `exp` regardless, so guessing long
    /// would produce a client that is confidently wrong right up until its
    /// session is closed under it. Guessing short costs one extra refresh.
    #[must_use]
    pub fn new(
        access_token: String,
        refresh_token: Option<String>,
        expires_in_secs: Option<u64>,
        now_ms: u64,
    ) -> Self {
        let lifetime_ms = expires_in_secs.unwrap_or(0).saturating_mul(1_000);
        Self {
            access_token,
            refresh_token,
            expires_at_ms: now_ms.saturating_add(lifetime_ms),
            renew_at_ms: now_ms.saturating_add(lifetime_ms * RENEW_AT_FRACTION / 100),
        }
    }

    /// Whether the access token is past its deadline at `now_ms`.
    #[must_use]
    pub const fn is_expired_at(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_at_ms
    }

    /// Whether a pre-emptive renewal is due at `now_ms`.
    #[must_use]
    pub const fn is_renewal_due_at(&self, now_ms: u64) -> bool {
        now_ms >= self.renew_at_ms
    }

    /// Milliseconds until renewal is due, or zero if it already is.
    #[must_use]
    pub const fn until_renewal_ms(&self, now_ms: u64) -> u64 {
        self.renew_at_ms.saturating_sub(now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    #[test]
    fn an_hour_long_token_renews_at_forty_five_minutes() {
        let c = Credentials::new("tok".into(), None, Some(3600), NOW);
        assert_eq!(c.expires_at_ms, NOW + 3_600_000);
        assert_eq!(c.renew_at_ms, NOW + 2_700_000);
        assert!(!c.is_renewal_due_at(NOW + 2_699_999));
        assert!(c.is_renewal_due_at(NOW + 2_700_000));
        assert!(!c.is_expired_at(NOW + 3_599_999));
        assert!(c.is_expired_at(NOW + 3_600_000));
    }

    /// Renewal has to come with runway. At 75% of an hour there are fifteen
    /// minutes left, which is what makes a failed refresh survivable.
    #[test]
    fn renewal_leaves_a_quarter_of_the_lifetime_in_hand() {
        let c = Credentials::new("tok".into(), None, Some(3600), NOW);
        assert_eq!(c.expires_at_ms - c.renew_at_ms, 900_000);
    }

    /// An issuer that omits `expires_in` must not be read as "never expires".
    /// The relay enforces the real `exp` either way, so the safe guess is the
    /// one that costs a refresh, not the one that costs a closed session.
    #[test]
    fn a_missing_expires_in_is_due_immediately_not_never() {
        let c = Credentials::new("tok".into(), None, None, NOW);
        assert!(c.is_renewal_due_at(NOW));
        assert!(c.is_expired_at(NOW));
        assert_eq!(c.until_renewal_ms(NOW), 0);
    }

    #[test]
    fn debug_never_prints_either_token() {
        let c = Credentials::new(
            "access-secret".into(),
            Some("refresh-secret".into()),
            Some(60),
            NOW,
        );
        let rendered = format!("{c:?}");
        assert!(!rendered.contains("access-secret"), "{rendered}");
        assert!(!rendered.contains("refresh-secret"), "{rendered}");
    }
}
