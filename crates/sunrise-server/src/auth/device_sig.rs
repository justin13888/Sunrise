//! `header_sig_v1` — per-request device binding.
//!
//! `docs/06-server/api.md` §device-binding: every authenticated request carries
//! `X-Sunrise-Device: <device_id>` plus `X-Sunrise-Device-Sig`, an Ed25519
//! detached signature "over the canonical request line, the `Date` header, and
//! a hash of the request body", verified under the device's registered signing
//! key.
//!
//! The bearer token proves *which account* is calling. It proves nothing about
//! which device, and a stolen bearer is replayable everywhere. The device
//! signature is what makes revocation meaningful: a revoked device's key is no
//! longer accepted even while its OIDC token remains valid at the IdP.
//!
//! # Canonical string
//!
//! The document does not pin a byte layout, so this does, and the constant is
//! the version tag clients must reproduce exactly:
//!
//! ```text
//! sunrise-device-sig-v1\n
//! <METHOD>\n                 uppercase, e.g. POST
//! <path>[?<query>]\n         origin-form target, exactly as sent
//! <Date>\n                   the Date header, verbatim
//! <blake3-256(body) as lowercase hex>
//! ```
//!
//! No trailing newline. BLAKE3 rather than SHA-256 because it is already the
//! workspace's hash everywhere else (blob chunk digests, id derivation), so
//! clients need no second hash implementation. An empty body hashes to
//! BLAKE3 of the empty string — absence is not a special case, which keeps a
//! body-stripping attacker from turning a signed POST into a signed GET.
//!
//! # Replay
//!
//! `Date` is checked against [`crate::state::Clock`] within
//! [`MAX_CLOCK_SKEW_SECS`]. It is signed, so it cannot be adjusted in flight,
//! and the window bounds how long a captured request stays replayable.

use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use thiserror::Error;

/// Header carrying the calling device's id.
pub const DEVICE_HEADER: &str = "x-sunrise-device";
/// Header carrying the detached Ed25519 request signature.
pub const DEVICE_SIG_HEADER: &str = "x-sunrise-device-sig";

/// Version tag; the first line of the canonical string.
pub const CANONICAL_V1_TAG: &str = "sunrise-device-sig-v1";

/// How far a request's `Date` may sit from server time, in seconds.
///
/// Five minutes matches the revocation-propagation window the API doc already
/// asks clients to tolerate, and is the usual allowance for unsynchronised
/// consumer clocks.
pub const MAX_CLOCK_SKEW_SECS: i64 = 300;

/// Why a device signature was rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DeviceSigError {
    /// A required header was absent.
    #[error("missing {0} header")]
    MissingHeader(&'static str),
    /// A header was present but not decodable.
    #[error("malformed {0} header")]
    MalformedHeader(&'static str),
    /// The registered device key is not a usable Ed25519 public key.
    #[error("device signing key is not a valid Ed25519 public key")]
    BadDeviceKey,
    /// `Date` is too far from server time.
    #[error("Date header is {skew}s from server time (max {MAX_CLOCK_SKEW_SECS}s)")]
    StaleDate {
        /// Absolute skew in seconds.
        skew: i64,
    },
    /// Signature did not verify.
    #[error("device signature does not verify")]
    BadSignature,
}

/// Build the canonical string a device signs.
#[must_use]
pub fn canonical_string(method: &str, path_and_query: &str, date: &str, body: &[u8]) -> String {
    let body_hash = blake3::hash(body).to_hex();
    format!("{CANONICAL_V1_TAG}\n{method}\n{path_and_query}\n{date}\n{body_hash}")
}

/// Decode a base64url-no-pad Ed25519 public key.
fn parse_verifying_key(device_pub_s: &str) -> Result<VerifyingKey, DeviceSigError> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(device_pub_s.trim())
        .map_err(|_| DeviceSigError::BadDeviceKey)?;
    let bytes: [u8; 32] = raw.try_into().map_err(|_| DeviceSigError::BadDeviceKey)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| DeviceSigError::BadDeviceKey)
}

/// Check that `date` is within [`MAX_CLOCK_SKEW_SECS`] of `now_ms`.
pub fn check_date(date: &str, now_ms: u64) -> Result<(), DeviceSigError> {
    let parsed = jiff::fmt::rfc2822::parse(date.trim())
        .map_err(|_| DeviceSigError::MalformedHeader("date"))?;
    let signed_at = parsed.timestamp().as_second();
    let now_secs = i64::try_from(now_ms / 1000).unwrap_or(i64::MAX);
    let skew = (now_secs - signed_at).abs();
    if skew > MAX_CLOCK_SKEW_SECS {
        return Err(DeviceSigError::StaleDate { skew });
    }
    Ok(())
}

/// Verify a `header_sig_v1` signature.
///
/// `device_pub_s` is the device's registered Ed25519 signing key, base64url
/// no-pad, as supplied to `POST /api/v1/devices`.
pub fn verify(
    device_pub_s: &str,
    signature_b64: &str,
    method: &str,
    path_and_query: &str,
    date: &str,
    body: &[u8],
    now_ms: u64,
) -> Result<(), DeviceSigError> {
    check_date(date, now_ms)?;
    let key = parse_verifying_key(device_pub_s)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(signature_b64.trim())
        .map_err(|_| DeviceSigError::MalformedHeader("x-sunrise-device-sig"))?;
    let bytes: [u8; 64] = raw
        .try_into()
        .map_err(|_| DeviceSigError::MalformedHeader("x-sunrise-device-sig"))?;
    let sig = Signature::from_bytes(&bytes);
    let canonical = canonical_string(method, path_and_query, date, body);
    key.verify(canonical.as_bytes(), &sig)
        .map_err(|_| DeviceSigError::BadSignature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// A deterministic device key. Signing is a pure function of the seed, so
    /// no ambient RNG is involved.
    fn device_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn b64(b: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    }

    /// 2024-01-01T00:00:00Z in ms.
    const NOW_MS: u64 = 1_704_067_200_000;
    const DATE: &str = "Mon, 01 Jan 2024 00:00:00 GMT";

    fn sign(sk: &SigningKey, method: &str, path: &str, date: &str, body: &[u8]) -> String {
        let canonical = canonical_string(method, path, date, body);
        b64(&sk.sign(canonical.as_bytes()).to_bytes())
    }

    #[test]
    fn a_correctly_signed_request_verifies() {
        let sk = device_key();
        let pk = b64(sk.verifying_key().as_bytes());
        let body = br#"{"nickname":"laptop"}"#;
        let sig = sign(&sk, "POST", "/api/v1/devices", DATE, body);
        assert_eq!(
            verify(&pk, &sig, "POST", "/api/v1/devices", DATE, body, NOW_MS),
            Ok(())
        );
    }

    /// Every component is covered: change any one and the signature must fail.
    #[test]
    fn each_signed_component_is_actually_bound() {
        let sk = device_key();
        let pk = b64(sk.verifying_key().as_bytes());
        let body = br#"{"nickname":"laptop"}"#;
        let sig = sign(&sk, "POST", "/api/v1/devices", DATE, body);

        // Method swapped.
        assert_eq!(
            verify(&pk, &sig, "DELETE", "/api/v1/devices", DATE, body, NOW_MS),
            Err(DeviceSigError::BadSignature)
        );
        // Path swapped — this is the one that stops a signed request for a
        // device the caller owns being replayed against another device's URL.
        assert_eq!(
            verify(
                &pk,
                &sig,
                "POST",
                "/api/v1/devices/OTHER",
                DATE,
                body,
                NOW_MS
            ),
            Err(DeviceSigError::BadSignature)
        );
        // Body swapped.
        assert_eq!(
            verify(
                &pk,
                &sig,
                "POST",
                "/api/v1/devices",
                DATE,
                br#"{"nickname":"attacker"}"#,
                NOW_MS
            ),
            Err(DeviceSigError::BadSignature)
        );
        // Body dropped entirely.
        assert_eq!(
            verify(&pk, &sig, "POST", "/api/v1/devices", DATE, b"", NOW_MS),
            Err(DeviceSigError::BadSignature)
        );
    }

    #[test]
    fn another_devices_key_does_not_verify() {
        let sk = device_key();
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let sig = sign(&sk, "GET", "/api/v1/devices", DATE, b"");
        assert_eq!(
            verify(
                &b64(other.verifying_key().as_bytes()),
                &sig,
                "GET",
                "/api/v1/devices",
                DATE,
                b"",
                NOW_MS
            ),
            Err(DeviceSigError::BadSignature)
        );
    }

    /// A captured request stops being replayable once it ages past the window,
    /// and the `Date` is inside the signature so it cannot be refreshed.
    #[test]
    fn a_captured_request_expires() {
        let sk = device_key();
        let pk = b64(sk.verifying_key().as_bytes());
        let sig = sign(&sk, "GET", "/api/v1/devices", DATE, b"");
        let ten_minutes_later = NOW_MS + 600_000;
        assert!(matches!(
            verify(
                &pk,
                &sig,
                "GET",
                "/api/v1/devices",
                DATE,
                b"",
                ten_minutes_later
            ),
            Err(DeviceSigError::StaleDate { .. })
        ));
        // Still fine one minute later.
        assert_eq!(
            verify(
                &pk,
                &sig,
                "GET",
                "/api/v1/devices",
                DATE,
                b"",
                NOW_MS + 60_000
            ),
            Ok(())
        );
    }

    /// A client whose clock runs fast is tolerated symmetrically.
    #[test]
    fn a_date_in_the_near_future_is_accepted() {
        let sk = device_key();
        let pk = b64(sk.verifying_key().as_bytes());
        let sig = sign(&sk, "GET", "/api/v1/devices", DATE, b"");
        assert_eq!(
            verify(
                &pk,
                &sig,
                "GET",
                "/api/v1/devices",
                DATE,
                b"",
                NOW_MS - 120_000
            ),
            Ok(())
        );
    }

    #[test]
    fn garbage_headers_are_rejected_without_panicking() {
        let sk = device_key();
        let pk = b64(sk.verifying_key().as_bytes());
        let sig = sign(&sk, "GET", "/x", DATE, b"");
        assert_eq!(
            verify(&pk, "not base64!", "GET", "/x", DATE, b"", NOW_MS),
            Err(DeviceSigError::MalformedHeader("x-sunrise-device-sig"))
        );
        assert_eq!(
            verify("not a key", &sig, "GET", "/x", DATE, b"", NOW_MS),
            Err(DeviceSigError::BadDeviceKey)
        );
        assert_eq!(
            verify(&pk, &sig, "GET", "/x", "yesterday", b"", NOW_MS),
            Err(DeviceSigError::MalformedHeader("date"))
        );
    }
}
