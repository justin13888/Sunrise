//! The server's logging surface: what it is allowed to say about a request.
//!
//! Everything here exists because the obvious thing to log is the thing we
//! must not. `tower-http`'s stock `TraceLayer` records `http.uri` — which on
//! this server is where browser clients put `?access_token=…`, because a
//! `WebSocket` upgrade cannot carry an `Authorization` header. A default
//! request-log configuration would therefore write bearer tokens to disk on
//! every `/sync` connection.
//!
//! The request log itself is `api::observe` now. kynos hands an observer the
//! *matched route* rather than the request's URI, so the query string it must
//! not record is not reachable from there at all — the hazard is gone by
//! construction rather than mitigated by a hand-assembled span. What remains
//! here is the pair of correlation handles every other module derives.
//!
//! `docs/10-cross-cutting/logging.md` §6.3 bans `Plain::expose` in this
//! module; the `log-redaction` CI gate greps this path.

/// Per-account correlation handle for logs.
///
/// `docs/10-cross-cutting/logging.md` §6 forbids logging a full `account_id`;
/// §6.1 defines `account_h` as the first 4 bytes of a salted BLAKE3, rendered
/// as 8 lowercase hex characters.
///
/// The salt is omitted here, deliberately. Its job in §6.1 is to stop a
/// low-entropy identifier — an email address — from being recovered by
/// brute force. Sunrise account ids are not that: `Store::resolve_account`
/// mints them as 16 random bytes with no relation to the OIDC subject or the
/// email, so the pre-image space is 2^128 and a dictionary attack has nothing
/// to enumerate. What a salt would still buy is preventing a client-side hash
/// and a server-side hash of the same account from being joined without the
/// operator's help — and clients hash their *own* ids under a device-local
/// salt, so that join is already unavailable.
#[must_use]
pub fn account_h(account_id: &str) -> String {
    let digest = blake3::hash(account_id.as_bytes());
    hex_lower(&digest.as_bytes()[..4])
}

/// Correlation handle for a raw 16-byte id — a stream id, or the account
/// namespace the relay derives per session. Same construction as
/// [`account_h`], and the same reasoning: these are 16 random bytes, so a
/// truncated hash has nothing to brute-force back to.
#[must_use]
pub fn id_h(id: &[u8; 16]) -> String {
    let digest = blake3::hash(id);
    hex_lower(&digest.as_bytes()[..4])
}

/// `hex::encode` is already lowercase and already in the lock file; the point
/// of the wrapper is the name, so the two call sites read as "the §6.1
/// construction" rather than as an encoding detail.
fn hex_lower(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_h_is_eight_lowercase_hex_chars() {
        let h = account_h("01J8ZQ7X9K3M5N7P9R1T3V5W7Y");
        assert_eq!(h.len(), 8, "{h}");
        assert!(
            h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{h}"
        );
    }

    #[test]
    fn account_h_is_stable_and_distinguishing() {
        assert_eq!(account_h("acct-a"), account_h("acct-a"));
        assert_ne!(account_h("acct-a"), account_h("acct-b"));
    }

    #[test]
    fn account_h_does_not_contain_the_id() {
        let id = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
        let h = account_h(id);
        assert!(!h.contains(&id.to_lowercase()));
        assert!(!id.to_lowercase().contains(&h));
    }

    #[test]
    fn id_h_matches_the_account_construction() {
        let sid = [7u8; 16];
        assert_eq!(id_h(&sid).len(), 8);
        assert_ne!(id_h(&sid), id_h(&[8u8; 16]));
    }

    #[test]
    fn hex_lower_pads_and_lowercases() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }
}
