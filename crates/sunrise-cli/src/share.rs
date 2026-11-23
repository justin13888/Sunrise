//! What `sunrise share` and `sunrise accept` read and write that is not a
//! vault call: the share key, the share's length, the reasons a link does
//! not open, an identity id as typed, which held share a word names, and
//! which relay answers leave a grant gone, kept pure so they are tested
//! without a vault or a relay.
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
use sunrise_core::keychain::HeldShare;
use sunrise_core::{CoreError, KeychainError};
use sunrise_crypto::ShareGrantError;
use sunrise_id::{EntityKind, EntityRef};
use sunrise_relay_client::grants::GrantError;

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

/// The identity id in `text`: 32 hex digits, either case, as `share list`
/// and `share` print it.
#[must_use]
pub fn parse_identity_id(text: &str) -> Option<[u8; 16]> {
    let text = text.trim();
    if text.len() != 32 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Whether a relay's answer to a withdrawal leaves the grant gone: it let
/// it go, or it holds no such grant for this account any more (withdrawn
/// already, expired, or declined by the other side), which the relay
/// answers the same way.
#[must_use]
pub fn withdrawal_settled(outcome: &Result<(), GrantError>) -> bool {
    matches!(outcome, Ok(()) | Err(GrantError::NotHeld))
}

/// The shares `typed` names among those this account holds: every one whose
/// owner's Stream id is `typed` (as a Stream ref), or, failing that, every
/// one whose sealed name is `typed`, ignoring case.
#[must_use]
pub fn held_matching<'a>(held: &'a [HeldShare], typed: &str) -> Vec<&'a HeldShare> {
    let typed = typed.trim().trim_start_matches('#').trim();
    if let Ok(id) = EntityRef::parse(typed, EntityKind::Stream) {
        return held.iter().filter(|h| h.stream_id == *id.bytes()).collect();
    }
    let folded = typed.to_lowercase();
    held.iter()
        .filter(|h| h.name.to_lowercase() == folded)
        .collect()
}

/// The owner's Stream a held share reads, as a Stream ref.
#[must_use]
pub fn held_stream_ref(share: &HeldShare) -> EntityRef {
    EntityRef::new(EntityKind::Stream, share.stream_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(stream: u8, name: &str) -> HeldShare {
        HeldShare {
            granter_identity_id: [1; 16],
            stream_id: [stream; 16],
            epoch: 2,
            name: name.to_owned(),
            color: "teal".to_owned(),
            expires_at_ms: None,
            accepted_at_ms: 3,
            relay_grant_ids: vec![[4; 16]],
        }
    }

    #[test]
    fn a_held_share_is_named_by_its_stream_id_or_its_name() {
        let shares = [held(7, "Garden"), held(8, "Kitchen"), held(9, "garden")];
        let by_ref = held_stream_ref(&shares[1]).to_str();
        let named = held_matching(&shares, &by_ref);
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].name, "Kitchen");
        assert_eq!(held_matching(&shares, &format!("#{by_ref}")).len(), 1);

        let named = held_matching(&shares, " GARDEN ");
        assert_eq!(named.len(), 2, "both Gardens, so the caller asks for an id");
        assert!(held_matching(&shares, "Attic").is_empty());
        let unheld = EntityRef::new(EntityKind::Stream, [5; 16]).to_str();
        assert!(held_matching(&shares, &unheld).is_empty());
    }

    #[test]
    fn an_identity_id_is_32_hex_digits() {
        let id = [0xab; 16];
        let hex = sunrise_client_core::ids::hex_id(&id);
        assert_eq!(parse_identity_id(&hex), Some(id));
        assert_eq!(parse_identity_id(&hex.to_uppercase()), Some(id));
        assert_eq!(parse_identity_id(&format!(" {hex}\n")), Some(id));
        assert_eq!(parse_identity_id(&hex[..30]), None);
        assert_eq!(parse_identity_id(&format!("{hex}00")), None);
        assert_eq!(parse_identity_id(&"zz".repeat(16)), None);
        assert_eq!(parse_identity_id(&"é".repeat(16)), None);
    }

    #[test]
    fn a_grant_the_relay_no_longer_holds_is_as_good_as_withdrawn() {
        assert!(withdrawal_settled(&Ok(())));
        assert!(withdrawal_settled(&Err(GrantError::NotHeld)));
        assert!(!withdrawal_settled(&Err(GrantError::Conflict(
            "x".to_owned()
        ))));
        assert!(!withdrawal_settled(&Err(GrantError::Request(
            "x".to_owned()
        ))));
    }

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
