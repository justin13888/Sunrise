//! `header_sig_v2` — per-request device binding over canonical JSON.
//!
//! Per [ADR-0022](../../../docs/11-adr/0022-device-signature-canonical-json.md).
//! The bearer token proves *which account* is calling and nothing about which
//! device; a stolen bearer is replayable from anywhere. The device signature is
//! what makes revocation meaningful — a revoked device's key stops being
//! accepted while its OIDC token is still valid at the `IdP`.
//!
//! # Why v2 exists
//!
//! v1 signed `blake3(body_bytes_as_received)`, which is why every authenticated
//! handler took the raw body: deserialising first would have left the signature
//! checking a re-serialisation. [ADR-0021](../../../docs/11-adr/0021-kynos-openapi-server.md)
//! adopted a framework with no raw-body extractor, by design — those are "the
//! holes through which aide and utoipa emit documents with silent gaps" — so v1
//! and an authoritative description cannot both survive.
//!
//! v2 signs the RFC 8785 canonical form of the request **value**. Both sides
//! compute it from the typed value: the client from what it is sending, the
//! server from what it parsed. Transport-level variation — key order,
//! whitespace, escaping — stops mattering, which is what JCS is for.
//!
//! # Canonical string
//!
//! ```text
//! sunrise-device-sig-v2\n
//! <METHOD>\n                 uppercase, e.g. POST
//! <path>[?<query>]\n         origin-form target, exactly as sent
//! <Date>\n                   the Date header, verbatim
//! <blake3-256(JCS(value)) as lowercase hex>
//! ```
//!
//! No trailing newline. BLAKE3 rather than SHA-256 because it is already the
//! workspace's hash everywhere else, so a client needs no second hash.
//!
//! A request with no body hashes the **empty byte string**, exactly as v1 did.
//! Absence is not a special case, which is what stops a body-stripping attacker
//! turning a signed `POST` into a signed `GET`.
//!
//! # Bodies that are not JSON
//!
//! A chunk upload is raw ciphertext, and there is no JSON value to canonicalize.
//! The rule generalises rather than needing a second scheme: what is hashed is
//! the body's **canonical form**, and for a binary body the bytes already are
//! it — there is no ordering, whitespace or escaping to normalise away. So
//! [`verify_canonical`] takes the bytes directly and JSON bodies reach it
//! through [`canonical_json`] first.
//!
//! This matters for what it does *not* let through. A signature made over a
//! chunk's bytes covers those exact bytes, so a relay that swapped ciphertext
//! between two uploads would produce a chunk whose signature no longer checks —
//! which is the property the blob store's hash verification and this signature
//! are asserting from two different directions.
//!
//! # The rule that makes re-serialisation safe to sign
//!
//! A signature over a re-serialisation verifies only if the parse is lossless,
//! and a silently-dropped field is exactly a lossy parse. So request bodies on
//! this surface **must** reject unknown fields (`#[serde(deny_unknown_fields)]`).
//! That turns what would be an inscrutable signature mismatch into a typed 400
//! naming the offending member.
//!
//! This is the opposite of the op log's rule, deliberately. Ops are end-to-end
//! encrypted peer data an *older* client must merge without destroying a newer
//! one's fields, so unknown keys round-trip verbatim there. API requests are a
//! versioned client/server contract in which the server is never behind the
//! client — it is the thing being deployed to — so an undocumented member is a
//! client bug or an attack, not a newer peer.
//!
//! # Replay
//!
//! `Date` is inside the signature, so it cannot be adjusted in flight, and
//! [`MAX_CLOCK_SKEW_SECS`] bounds how long a captured request stays replayable.

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use thiserror::Error;

/// Header carrying the calling device's id.
pub const DEVICE_HEADER: &str = "x-sunrise-device";
/// Header carrying the detached Ed25519 request signature.
pub const DEVICE_SIG_HEADER: &str = "x-sunrise-device-sig";

/// Version tag; the first line of the canonical string.
pub const CANONICAL_V2_TAG: &str = "sunrise-device-sig-v2";

/// The mode string `GET /api/v1/meta` advertises for this scheme.
pub const BINDING_MODE: &str = "header_sig_v2";

/// How far a request's `Date` may sit from server time, in seconds.
///
/// Five minutes is the usual allowance for unsynchronised consumer clocks, and
/// matches the revocation-propagation window the API doc asks clients to
/// tolerate.
pub const MAX_CLOCK_SKEW_SECS: i64 = 300;

/// Why a device signature was rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SigError {
    /// A required header was absent.
    #[error("missing {0} header")]
    MissingHeader(&'static str),
    /// A header was present but not decodable.
    #[error("malformed {0} header")]
    MalformedHeader(&'static str),
    /// The registered device key is not a usable Ed25519 public key.
    #[error("device signing key is not a valid Ed25519 public key")]
    BadDeviceKey,
    /// The value could not be canonicalized.
    ///
    /// Reachable for a map with non-string keys. Note that it is *not* reached
    /// for a non-finite float: `serde_jcs` follows `serde_json` and writes
    /// `null`, which is why no request type on this surface carries an `f64`.
    /// See `a_non_finite_float_collapses_to_null_which_is_why_floats_are_not_signed`.
    #[error("value is not canonicalizable as JSON: {0}")]
    NotCanonicalizable(String),
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

/// The RFC 8785 canonical JSON encoding of `value`.
///
/// # Errors
/// [`SigError::NotCanonicalizable`] when the value cannot be represented — a
/// non-finite float, or a map with non-string keys.
pub fn canonical_json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, SigError> {
    serde_jcs::to_vec(value).map_err(|e| SigError::NotCanonicalizable(e.to_string()))
}

/// Build the canonical string a device signs.
///
/// `canonical_body` is [`canonical_json`] of the request value, or empty for a
/// request that carries no body.
#[must_use]
pub fn canonical_string(
    method: &str,
    path_and_query: &str,
    date: &str,
    canonical_body: &[u8],
) -> String {
    let body_hash = blake3::hash(canonical_body).to_hex();
    format!("{CANONICAL_V2_TAG}\n{method}\n{path_and_query}\n{date}\n{body_hash}")
}

/// Sign a request. The client half.
///
/// # Errors
/// [`SigError::NotCanonicalizable`] when `value` cannot be canonicalized.
pub fn sign<T: serde::Serialize>(
    signing_key: &SigningKey,
    method: &str,
    path_and_query: &str,
    date: &str,
    value: Option<&T>,
) -> Result<String, SigError> {
    sign_with(
        |msg| signing_key.sign(msg).to_bytes(),
        method,
        path_and_query,
        date,
        value,
    )
}

/// [`sign`], for a client that does not hold a [`SigningKey`] to hand over.
///
/// The device's Ed25519 secret lives inside the vault keychain and is never
/// handed out — `sunrise_core::Core` signs on a caller's behalf and returns the
/// 64 bytes. `sign` would need the key itself, so this takes the *operation*
/// instead: the caller supplies something that turns a message into a
/// signature, and the canonical string, the hash and the base64url encoding
/// stay here, where the verifier's half of each already is.
///
/// # Errors
/// [`SigError::NotCanonicalizable`] when `value` cannot be canonicalized.
pub fn sign_with<T: serde::Serialize>(
    sign_bytes: impl FnOnce(&[u8]) -> [u8; 64],
    method: &str,
    path_and_query: &str,
    date: &str,
    value: Option<&T>,
) -> Result<String, SigError> {
    let body = match value {
        Some(v) => canonical_json(v)?,
        None => Vec::new(),
    };
    let canonical = canonical_string(method, path_and_query, date, &body);
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sign_bytes(canonical.as_bytes())))
}

/// Encode an Ed25519 public key the way `device_pub_s` is registered and read.
///
/// The inverse of `parse_verifying_key`, and it sits beside it deliberately.
/// `POST /api/v1/devices` refuses anything this does not produce, and the one
/// caller that registered a device got the encoding wrong — it sent the hex of
/// a 16-byte device *id*, which decodes to 24 bytes and can never be a key, so
/// every request that device signed afterwards was a 401 nobody could act on.
/// A client that reaches for this rather than formatting bytes itself cannot
/// repeat that.
#[must_use]
pub fn device_pub_b64(device_pub_s: &[u8; 32]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(device_pub_s)
}

/// Format `now_ms` as the `Date` header this scheme signs.
///
/// Takes the instant rather than reading one: `clippy.toml`'s disallowed-methods
/// list and the CI determinism gate forbid an ambient clock, so the caller
/// passes whatever its own injected clock said. That is also what makes a
/// skewed-clock test possible at all — a helper that called `SystemTime::now`
/// could only ever be tested against the truth.
#[must_use]
pub fn date_header(now_ms: u64) -> String {
    let secs = i64::try_from(now_ms / 1000).unwrap_or(i64::MAX);
    jiff::Timestamp::from_second(secs)
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
        .strftime("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

/// How far `date` sits from `now_ms`, in seconds, or `None` if it is not an
/// RFC 2822 date.
///
/// Exposed for the client half: a refused signature is most often a clock, and
/// "your clock is 412 s from the relay's" is a thing a user can fix where a
/// bare 401 is not. Signed rather than absolute, so a client can say *which*
/// way it is wrong.
#[must_use]
pub fn skew_secs(date: &str, now_ms: u64) -> Option<i64> {
    let parsed = jiff::fmt::rfc2822::parse(date.trim()).ok()?;
    let now_secs = i64::try_from(now_ms / 1000).unwrap_or(i64::MAX);
    Some(now_secs - parsed.timestamp().as_second())
}

/// Decode a base64url-no-pad Ed25519 public key.
fn parse_verifying_key(device_pub_s: &str) -> Result<VerifyingKey, SigError> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(device_pub_s.trim())
        .map_err(|_| SigError::BadDeviceKey)?;
    let bytes: [u8; 32] = raw.try_into().map_err(|_| SigError::BadDeviceKey)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| SigError::BadDeviceKey)
}

/// Check that `date` is within [`MAX_CLOCK_SKEW_SECS`] of `now_ms`.
///
/// # Errors
/// [`SigError::MalformedHeader`] if `date` is not an RFC 2822 date, or
/// [`SigError::StaleDate`] if it is outside the window.
pub fn check_date(date: &str, now_ms: u64) -> Result<(), SigError> {
    let skew = skew_secs(date, now_ms)
        .ok_or(SigError::MalformedHeader("date"))?
        .abs();
    if skew > MAX_CLOCK_SKEW_SECS {
        return Err(SigError::StaleDate { skew });
    }
    Ok(())
}

/// Verify a `header_sig_v2` signature against an already-canonicalized body.
///
/// The server calls this after its framework has parsed the body into the
/// operation's declared type, with `canonical_body` recomputed from that value
/// by [`canonical_json`] — which is sound precisely because the request types
/// reject unknown fields, so nothing was dropped between the wire and here.
///
/// # Errors
/// See [`SigError`]. Every failure mode is distinguishable to the *server* for
/// logging; the caller is responsible for collapsing them to one status before
/// they reach a client.
pub fn verify_canonical(
    device_pub_s: &str,
    signature_b64: &str,
    method: &str,
    path_and_query: &str,
    date: &str,
    canonical_body: &[u8],
    now_ms: u64,
) -> Result<(), SigError> {
    check_date(date, now_ms)?;
    let key = parse_verifying_key(device_pub_s)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(signature_b64.trim())
        .map_err(|_| SigError::MalformedHeader(DEVICE_SIG_HEADER))?;
    let bytes: [u8; 64] = raw
        .try_into()
        .map_err(|_| SigError::MalformedHeader(DEVICE_SIG_HEADER))?;
    let sig = Signature::from_bytes(&bytes);
    let canonical = canonical_string(method, path_and_query, date, canonical_body);
    key.verify(canonical.as_bytes(), &sig)
        .map_err(|_| SigError::BadSignature)
}

/// Verify against a value, canonicalizing it first.
///
/// # Errors
/// See [`SigError`].
pub fn verify<T: serde::Serialize>(
    device_pub_s: &str,
    signature_b64: &str,
    method: &str,
    path_and_query: &str,
    date: &str,
    value: Option<&T>,
    now_ms: u64,
) -> Result<(), SigError> {
    let body = match value {
        Some(v) => canonical_json(v)?,
        None => Vec::new(),
    };
    verify_canonical(
        device_pub_s,
        signature_b64,
        method,
        path_and_query,
        date,
        &body,
        now_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    struct Body {
        zeta: String,
        alpha: u32,
    }

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn pub_b64(k: &SigningKey) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(k.verifying_key().to_bytes())
    }

    const DATE: &str = "Mon, 31 Aug 2026 00:00:00 GMT";
    /// The instant `DATE` names, in epoch milliseconds.
    const NOW_MS: u64 = 1_788_134_400_000;

    fn body() -> Body {
        Body {
            zeta: "z".into(),
            alpha: 1,
        }
    }

    /// The registration bug, pinned from both directions.
    ///
    /// `device_pub_b64` must produce exactly what `parse_verifying_key` reads
    /// back, and the shape that shipped instead — hex of a 16-byte device id —
    /// must not survive the decode. Nothing asserted either before: the encoder
    /// did not exist, so every caller invented its own and one of them invented
    /// the wrong one.
    #[test]
    fn a_device_public_key_round_trips_through_its_wire_form() {
        let k = key();
        let encoded = device_pub_b64(&k.verifying_key().to_bytes());
        assert_eq!(encoded, pub_b64(&k));
        assert_eq!(
            parse_verifying_key(&encoded).expect("the encoder's output must parse"),
            k.verifying_key()
        );

        // 32 hex characters of a 16-byte id: the right length for a *string*
        // and 24 bytes once base64url-decoded, so it can never be a key.
        let device_id_hex = "0102030405060708090a0b0c0d0e0f10";
        assert_eq!(device_id_hex.len(), 32);
        assert_eq!(
            parse_verifying_key(device_id_hex),
            Err(SigError::BadDeviceKey)
        );
    }

    /// A client that holds its key behind an abstraction signs identically to
    /// one that hands the key over, which is what lets the core keep `D_S_priv`
    /// and still produce a request the relay accepts.
    #[test]
    fn signing_through_a_closure_matches_signing_with_the_key() {
        let k = key();
        let direct = sign(&k, "POST", "/api/v1/sync/ops", DATE, Some(&body())).unwrap();
        let indirect = sign_with(
            |msg| k.sign(msg).to_bytes(),
            "POST",
            "/api/v1/sync/ops",
            DATE,
            Some(&body()),
        )
        .unwrap();
        assert_eq!(direct, indirect);
        verify(
            &pub_b64(&k),
            &indirect,
            "POST",
            "/api/v1/sync/ops",
            DATE,
            Some(&body()),
            NOW_MS,
        )
        .expect("the closure-signed request must verify");
    }

    /// The `Date` a client emits is the one the verifier parses back, and
    /// `NOW_MS` is `DATE` — so this pins the format against the constant the
    /// rest of the module already trusts.
    #[test]
    fn the_date_header_round_trips_through_the_skew_check() {
        assert_eq!(date_header(NOW_MS), DATE);
        assert_eq!(skew_secs(DATE, NOW_MS), Some(0));
        check_date(&date_header(NOW_MS), NOW_MS).expect("a freshly formatted date is not stale");
    }

    /// Signed, not absolute: a client ahead of the relay and a client behind it
    /// need different advice.
    #[test]
    fn skew_is_signed_so_a_client_can_say_which_way_it_is_wrong() {
        assert_eq!(skew_secs(DATE, NOW_MS + 90_000), Some(90));
        assert_eq!(skew_secs(DATE, NOW_MS - 90_000), Some(-90));
        assert_eq!(skew_secs("not a date", NOW_MS), None);
    }

    #[test]
    fn a_signed_request_verifies() {
        let k = key();
        let sig = sign(&k, "POST", "/api/v1/accounts", DATE, Some(&body())).unwrap();
        verify(
            &pub_b64(&k),
            &sig,
            "POST",
            "/api/v1/accounts",
            DATE,
            Some(&body()),
            NOW_MS,
        )
        .expect("a signature made over this value must verify against it");
    }

    /// The whole point of signing the value rather than the octets: the client
    /// may serialize its fields in any order, and the signature still verifies.
    ///
    /// `Body` declares `zeta` before `alpha`, so serde emits it that way; JCS
    /// sorts to `alpha, zeta`. If either side signed declaration order this
    /// would pass by accident, so the assertion below pins the canonical bytes
    /// as well as the verification.
    #[test]
    fn canonicalization_is_independent_of_field_order() {
        let canonical = canonical_json(&body()).unwrap();
        assert_eq!(
            String::from_utf8(canonical).unwrap(),
            r#"{"alpha":1,"zeta":"z"}"#,
            "JCS must sort keys, so the wire order the client happened to emit \
             cannot change what gets signed"
        );
    }

    /// RFC 8785 §3.2.3: keys sort by UTF-16 code unit, and non-ASCII escapes
    /// are unescaped in the output. This is the case a naive byte-wise sort
    /// gets wrong, so it is worth pinning rather than trusting.
    #[test]
    fn canonicalization_matches_rfc_8785_ordering() {
        let v: serde_json::Value = serde_json::json!({
            "\u{20ac}": "euro",
            "\u{00e9}": "e-acute",
            "a": "ascii",
        });
        let out = String::from_utf8(canonical_json(&v).unwrap()).unwrap();
        assert_eq!(
            out,
            "{\"a\":\"ascii\",\"\u{00e9}\":\"e-acute\",\"\u{20ac}\":\"euro\"}"
        );
    }

    #[test]
    fn a_different_value_does_not_verify() {
        let k = key();
        let sig = sign(&k, "POST", "/api/v1/accounts", DATE, Some(&body())).unwrap();
        let tampered = Body {
            zeta: "z".into(),
            alpha: 2,
        };
        assert_eq!(
            verify(
                &pub_b64(&k),
                &sig,
                "POST",
                "/api/v1/accounts",
                DATE,
                Some(&tampered),
                NOW_MS,
            ),
            Err(SigError::BadSignature)
        );
    }

    /// Method and target are inside the signature, so a captured POST cannot be
    /// replayed at another route or with another verb.
    #[test]
    fn method_and_target_are_bound() {
        let k = key();
        let sig = sign(&k, "POST", "/api/v1/accounts", DATE, Some(&body())).unwrap();
        for (m, p) in [
            ("PUT", "/api/v1/accounts"),
            ("POST", "/api/v1/devices"),
            ("POST", "/api/v1/accounts?x=1"),
        ] {
            assert_eq!(
                verify(&pub_b64(&k), &sig, m, p, DATE, Some(&body()), NOW_MS),
                Err(SigError::BadSignature),
                "{m} {p} must not verify under a signature made for POST /api/v1/accounts"
            );
        }
    }

    /// Absence is not a special case: a bodyless request hashes the empty
    /// string, so stripping a body from a signed request does not produce a
    /// signature that verifies as a bodyless one.
    #[test]
    fn stripping_the_body_does_not_verify() {
        let k = key();
        let sig = sign(&k, "POST", "/api/v1/accounts", DATE, Some(&body())).unwrap();
        assert_eq!(
            verify::<Body>(
                &pub_b64(&k),
                &sig,
                "POST",
                "/api/v1/accounts",
                DATE,
                None,
                NOW_MS,
            ),
            Err(SigError::BadSignature)
        );
    }

    #[test]
    fn a_stale_date_is_refused_before_the_signature_is_checked() {
        let k = key();
        let sig = sign(&k, "POST", "/api/v1/accounts", DATE, Some(&body())).unwrap();
        let late = NOW_MS + (MAX_CLOCK_SKEW_SECS as u64 + 1) * 1000;
        assert!(matches!(
            verify(
                &pub_b64(&k),
                &sig,
                "POST",
                "/api/v1/accounts",
                DATE,
                Some(&body()),
                late,
            ),
            Err(SigError::StaleDate { .. })
        ));
    }

    #[test]
    fn a_signature_from_another_device_does_not_verify() {
        let mine = key();
        let theirs = SigningKey::from_bytes(&[9u8; 32]);
        let sig = sign(&theirs, "POST", "/api/v1/accounts", DATE, Some(&body())).unwrap();
        assert_eq!(
            verify(
                &pub_b64(&mine),
                &sig,
                "POST",
                "/api/v1/accounts",
                DATE,
                Some(&body()),
                NOW_MS,
            ),
            Err(SigError::BadSignature)
        );
    }

    /// Floats are outside what this surface signs, and the reason is recorded
    /// rather than assumed.
    ///
    /// RFC 8785 §3.2.2.3 defines a serialization only for finite numbers, and
    /// `serde_jcs` does not reject a non-finite one — it is `serde_json`'s
    /// behaviour underneath, which turns it into `null`. A value that
    /// canonicalizes to something other than itself is exactly the lossy-parse
    /// hazard this module's header warns about: two different values would sign
    /// identically.
    ///
    /// The defence is structural rather than a runtime check. No request type
    /// on this surface carries an `f64`, and the same prohibition already holds
    /// one layer down — `sunrise_cbor::CborValue` refuses floats on decode. This
    /// test pins the hazard so that adding a float field is a decision someone
    /// makes against a documented consequence.
    #[test]
    fn a_non_finite_float_collapses_to_null_which_is_why_floats_are_not_signed() {
        #[derive(Serialize)]
        struct Bad {
            n: f64,
        }
        let inf = canonical_json(&Bad { n: f64::INFINITY }).expect("jcs does not reject it");
        let nan = canonical_json(&Bad { n: f64::NAN }).expect("jcs does not reject it");
        assert_eq!(String::from_utf8(inf.clone()).unwrap(), r#"{"n":null}"#);
        assert_eq!(
            inf, nan,
            "two distinguishable values canonicalize identically, so a float              field would make the signature ambiguous"
        );
    }
}
