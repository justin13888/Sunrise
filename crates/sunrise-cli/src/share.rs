//! What `sunrise share` and `sunrise accept` read and write that is not a
//! vault call: the share key, the share's length, and the reasons a link does
//! not open, kept pure so they are tested without a vault or a relay.
//!
//! # The share key
//!
//! The owner needs two of the recipient's public keys to share with it
//! (`docs/03-crypto/sharing-with-others.md` §Granting access): `ID_S_pub`,
//! which names the identity, and `ID_D_pub`, which the grant is sealed to.
//! There is no lookup of either on the relay, so the recipient hands both
//! over out of band as one string: base64url, unpadded, of
//! `ID_S_pub (32) || ID_D_pub (32)`. Both are public; the string is not a
//! credential.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sunrise_core::{CoreError, KeychainError};
use sunrise_crypto::ShareGrantError;

use crate::i18n::strings;

/// This account's share key, from its `ID_S_pub` and `ID_D_pub`.
#[must_use]
pub fn encode_share_key(id_s_pub: &[u8; 32], id_d_pub: &[u8; 32]) -> String {
    let mut raw = [0u8; 64];
    raw[..32].copy_from_slice(id_s_pub);
    raw[32..].copy_from_slice(id_d_pub);
    URL_SAFE_NO_PAD.encode(raw)
}

/// The `(ID_S_pub, ID_D_pub)` a share key carries, or `None` for text that
/// is not canonical unpadded base64url of exactly 64 bytes.
#[must_use]
pub fn decode_share_key(text: &str) -> Option<([u8; 32], [u8; 32])> {
    let raw = URL_SAFE_NO_PAD.decode(text.trim()).ok()?;
    let raw: [u8; 64] = raw.try_into().ok()?;
    let mut s = [0u8; 32];
    let mut d = [0u8; 32];
    s.copy_from_slice(&raw[..32]);
    d.copy_from_slice(&raw[32..]);
    Some((s, d))
}

/// One day, in milliseconds.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// When a share of `days` days, made at `now_ms`, ends; `None` for a `days`
/// that is not a whole number of at least one, or an end past the clock's
/// range.
#[must_use]
pub fn expiry_after_days(now_ms: u64, days: &str) -> Option<(u32, u64)> {
    let n: u32 = days.trim().parse().ok().filter(|n| *n >= 1)?;
    let end = u64::from(n).checked_mul(DAY_MS)?.checked_add(now_ms)?;
    // The relay stores the expiry as a signed 64-bit integer.
    i64::try_from(end).ok()?;
    Some((n, end))
}

/// The sentence for a link that did not open for this account, worded by
/// what the person can do about it.
#[must_use]
pub fn open_refusal(e: &CoreError) -> String {
    match e {
        CoreError::Keychain(KeychainError::IdentitySecretAbsent) => {
            strings::share::no_identity_key()
        }
        CoreError::Keychain(KeychainError::ShareGrant(ShareGrantError::OtherRecipient)) => {
            strings::share::other_recipient()
        }
        CoreError::Keychain(KeychainError::ShareGrant(ShareGrantError::BadSignature)) => {
            strings::share::bad_signature()
        }
        CoreError::Keychain(KeychainError::ShareGrant(ShareGrantError::Expired)) => {
            strings::share::expired()
        }
        other => strings::share::unopenable(&other.to_string()),
    }
}

/// The sentence for a claimed relay grant whose Stream is not the link's.
///
/// The claim has already bound the grant to this account, so `accept`
/// declines it again before refusing; `declined` is that withdrawal's
/// outcome, and where it failed the sentence says the grant is still held
/// rather than that nothing was accepted. `grant` is the grant's id in hex.
#[must_use]
pub fn mismatch_refusal(declined: Result<(), String>, grant: &str) -> String {
    match declined {
        Ok(()) => strings::share::mismatch(),
        Err(e) => strings::share::mismatch_held(&e, grant),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mismatch_says_nothing_is_accepted_only_once_declined() {
        let declined = mismatch_refusal(Ok(()), "0a0b");
        assert!(declined.contains("nothing is accepted"), "{declined}");

        let held = mismatch_refusal(Err("relay unreachable".to_owned()), "0a0b");
        assert!(!held.contains("nothing is accepted"), "{held}");
        assert!(held.contains("still holds relay grant 0a0b"), "{held}");
        assert!(held.contains("relay unreachable"), "{held}");
    }

    #[test]
    fn a_share_key_round_trips_and_nothing_else_reads_as_one() {
        let s = [0xfb; 32];
        let d = [0x3e; 32];
        let key = encode_share_key(&s, &d);
        assert!(!key.contains(['+', '/', '=']), "{key}");
        assert_eq!(decode_share_key(&key), Some((s, d)));
        assert_eq!(decode_share_key(&format!("  {key}\n")), Some((s, d)));

        assert_eq!(decode_share_key(&URL_SAFE_NO_PAD.encode([1u8; 63])), None);
        assert_eq!(decode_share_key(&URL_SAFE_NO_PAD.encode([1u8; 65])), None);
        assert_eq!(decode_share_key(&format!("{key}=")), None);
        assert_eq!(decode_share_key("not a key"), None);
    }

    #[test]
    fn a_share_lasts_whole_days_from_now() {
        assert_eq!(expiry_after_days(1_000, "3"), Some((3, 1_000 + 3 * DAY_MS)));
        assert_eq!(expiry_after_days(0, "0"), None);
        assert_eq!(expiry_after_days(0, "-1"), None);
        assert_eq!(expiry_after_days(0, "1.5"), None);
        assert_eq!(expiry_after_days(0, "soon"), None);
        assert_eq!(expiry_after_days(u64::MAX - 1, "1"), None);
        assert_eq!(expiry_after_days(i64::MAX as u64 - DAY_MS + 1, "1"), None);
    }

    #[test]
    fn each_refusal_says_what_to_do() {
        let share = |e| CoreError::Keychain(KeychainError::ShareGrant(e));
        assert_eq!(
            open_refusal(&share(ShareGrantError::OtherRecipient)),
            strings::share::other_recipient()
        );
        assert_eq!(
            open_refusal(&share(ShareGrantError::BadSignature)),
            strings::share::bad_signature()
        );
        assert_eq!(
            open_refusal(&share(ShareGrantError::Expired)),
            strings::share::expired()
        );
        assert_eq!(
            open_refusal(&CoreError::Keychain(KeychainError::IdentitySecretAbsent)),
            strings::share::no_identity_key()
        );
        assert!(open_refusal(&CoreError::Closed).contains("does not open"));
    }
}
