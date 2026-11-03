//! `DeviceCert` — signed certificate binding a device to an identity.
//!
//! Per `docs/03-crypto/identity-and-device-keys.md`:
//!
//! ```cddl
//! DeviceCert = {
//!     1: bstr,                         ; body_bytes: the encoded DeviceCertBody
//!     2: bstr .size 64                 ; sig = Ed25519_sign(ID_S_priv,
//!                                      ;   "sunrise.device_cert.v1" || BLAKE3(body_bytes, 32))
//! }
//!
//! DeviceCertBody = {
//!     1: uint,                         ; v (= 1)
//!     2: bstr .size 16,                ; device_id
//!     3: bstr .size 32,                ; D_S_pub
//!     4: bstr .size 32,                ; D_D_pub
//!     5: bstr .size 16,                ; identity_id_bytes
//!     6: uint,                         ; created_at (ms since epoch)
//!     7: tstr .size (1..64),           ; nickname (utf-8)
//!     8: tstr,                         ; platform
//! }
//! ```
//!
//! # The body travels as a byte string, and the signature covers those bytes
//!
//! `body_bytes` is an opaque `bstr` in the outer map rather than a nested CBOR
//! map, and that is the whole of what makes verification sound here.
//!
//! The body used to be a nested map, so [`DeviceCert::from_cbor`] parsed it
//! into [`DeviceCertInner`] and the parsed struct was all that survived.
//! [`DeviceCert::verify`] then re-encoded that struct and checked the signature
//! over **the re-encoding**, not over the bytes that arrived. Any asymmetry
//! between parse and re-encode was therefore a verification gap: a body whose
//! keys arrived in a different order, or with a non-minimal integer, decoded
//! identically and re-encoded canonically, so it verified under a signature
//! taken over different bytes. Two distinct blobs, one signature, both
//! "valid" — and `roster_digest` hashes the *blob*, so the two were not
//! interchangeable downstream even though verification said they were.
//!
//! Refusing trailing bytes closed one route into that. It did not close the
//! class, because the class is "the verifier never saw the input". Carrying the
//! body as a `bstr` removes the re-encoding entirely: there is exactly one byte
//! sequence, it is what was signed, it is what is hashed, and it is what
//! `to_cbor` writes back out. The same construction COSE uses for its protected
//! header, for the same reason.
//!
//! Canonicity of the body's *interior* is still not enforced — the workspace
//! does not enforce it anywhere (see the note in
//! `docs/03-crypto/data-encryption-format.md`) — and no longer needs to be. A
//! non-canonical body is simply a different byte string, signed or not signed
//! on its own merits.

use crate::identity::identity_id_from_pub;
use crate::keys::{verify_ed25519, IdentitySigningKeyPair};
use ciborium::value::{Integer, Value};
use codec::{body_from_cbor, body_to_cbor};
use subtle::ConstantTimeEq;
use thiserror::Error;

mod codec;

const DEVICE_CERT_DOMAIN: &[u8] = b"sunrise.device_cert.v1";

/// The upper bound on a device nickname, in **bytes** of UTF-8.
///
/// This is the number the CDDL at the top of this module writes as
/// `tstr .size (1..64)`, and it is the one the code reads: this module's
/// encoder and decoder both check against this constant rather than against a
/// literal, so the two cannot drift apart.
///
/// The bound is not only this crate's. `sunrise-server` refuses a
/// device-registration whose `nickname` exceeds the same number, and the two
/// have to agree in both directions: a server bound *below* this one accepts
/// certs it will not register, and a server bound *above* it registers a device
/// whose cert no peer's decoder will read. Nothing in the type system links
/// them, so `sunrise-e2e/tests/nickname_bound_agreement.rs` — the one crate that
/// already depends on both — asserts the equality and exercises the boundary
/// against the real handler.
pub const MAX_NICKNAME_BYTES: usize = 64;

/// Parsed inner body of a `DeviceCert`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCertInner {
    /// Cert version (currently 1).
    pub v: u32,
    /// Device id (16 bytes).
    pub device_id: [u8; 16],
    /// Device's Ed25519 public key.
    pub d_s_pub: [u8; 32],
    /// Device's X25519 public key.
    pub d_d_pub: [u8; 32],
    /// Identity id this device belongs to.
    pub identity_id: [u8; 16],
    /// Cert creation time (ms since epoch).
    pub created_at_ms: u64,
    /// Human-readable nickname, 1..=[`MAX_NICKNAME_BYTES`] **bytes** of UTF-8
    /// (not characters).
    ///
    /// The CDDL above says `tstr .size (1..64)`, and `.size` on a CDDL `tstr`
    /// bounds bytes. `sunrise-server` applies the same upper bound to the
    /// `nickname` field of a device-registration request (its own
    /// `MAX_NICKNAME_BYTES`, asserted equal to this one), so a character bound
    /// here would let a client build a cert naming a nickname its own
    /// registration call cannot carry.
    ///
    /// The *lower* bounds deliberately differ, and this is not the same bound:
    /// this codec refuses only `""`, because a cert is a wire object and
    /// whitespace is a valid `tstr`, while the server additionally refuses an
    /// all-whitespace nickname as a presentation rule on its request field.
    ///
    /// The server never parses or verifies the cert — it stores it as opaque
    /// text — so nothing downstream re-checks these bytes. This decoder is the
    /// only thing standing behind them.
    pub nickname: String,
    /// Platform string (e.g., "macos15", "ios18", "linux-x86_64").
    pub platform: String,
}

/// Outer device cert: the encoded body, its parse, and the Ed25519 signature.
///
/// Constructed only by [`DeviceCert::issue`] and [`DeviceCert::from_cbor`],
/// which is what `body_bytes` being private enforces: the field is the one the
/// signature covers, and a caller able to set `body` and `body_bytes`
/// independently could build a cert whose parse says one thing and whose signed
/// bytes say another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCert {
    /// The body fields, as parsed from [`Self::body_bytes`].
    ///
    /// A convenience view. It is **not** what the signature covers, and it is
    /// not re-encoded to check one: see this module's header.
    pub body: DeviceCertInner,
    /// 64-byte Ed25519 signature over `DEVICE_CERT_DOMAIN || BLAKE3(body_bytes)`.
    pub sig: [u8; 64],
    /// The exact bytes of the encoded body: what was signed, and — for a cert
    /// that arrived from somewhere — what arrived.
    body_bytes: Vec<u8>,
}

/// Errors produced by [`DeviceCert`] operations.
#[derive(Debug, Error)]
pub enum DeviceCertError {
    /// CBOR encode/decode failure.
    #[error("CBOR error: {0}")]
    Cbor(String),
    /// Field shape error (wrong byte length, missing field, …).
    #[error("invalid device cert field: {0}")]
    BadField(&'static str),
    /// Ed25519 signature verify failed.
    #[error("device cert signature verify failed")]
    SigVerify,
    /// The cert's `identity_id` is not the one derived from the identity key
    /// that signed it.
    #[error("device cert identity_id does not match the signing identity")]
    IdentityMismatch,
    /// Body validation failed (nickname length, etc.).
    #[error("device cert validation failed: {0}")]
    Validation(&'static str),
}

/// `DEVICE_CERT_DOMAIN || BLAKE3(body_bytes)`.
///
/// Takes the encoded body rather than the parsed one, and is infallible for
/// that reason: there is nothing left to encode and so nothing left to fail.
/// The old signature took `&DeviceCertInner` and encoded it here, which is
/// precisely where the re-encoding entered verification.
fn sig_input_bytes(body_bytes: &[u8]) -> Vec<u8> {
    let body_hash = blake3::hash(body_bytes);
    let mut out = Vec::with_capacity(DEVICE_CERT_DOMAIN.len() + body_hash.as_bytes().len());
    out.extend_from_slice(DEVICE_CERT_DOMAIN);
    out.extend_from_slice(body_hash.as_bytes());
    out
}

impl DeviceCert {
    /// Build a new device cert; sign the body with the identity signing key.
    ///
    /// # Errors
    /// [`DeviceCertError::Validation`] when `body.nickname` is empty or longer
    /// than [`MAX_NICKNAME_BYTES`] bytes, [`DeviceCertError::Cbor`] when the body
    /// fails to encode.
    pub fn issue(
        body: DeviceCertInner,
        identity_signing: &IdentitySigningKeyPair,
    ) -> Result<Self, DeviceCertError> {
        let body_bytes = body_to_cbor(&body)?;
        let sig = identity_signing.sign(&sig_input_bytes(&body_bytes));
        Ok(Self {
            body,
            sig,
            body_bytes,
        })
    }

    /// The exact bytes the signature covers.
    ///
    /// For a cert from [`Self::from_cbor`] these are the bytes that arrived,
    /// byte for byte, and not a re-encoding of the parse.
    #[must_use]
    pub fn body_bytes(&self) -> &[u8] {
        &self.body_bytes
    }

    /// Verify the cert against the identity's `ID_S_pub`.
    ///
    /// # Errors
    /// Returns [`DeviceCertError::SigVerify`] on signature mismatch.
    pub fn verify(&self, id_s_pub: &[u8; 32]) -> Result<(), DeviceCertError> {
        let input = sig_input_bytes(&self.body_bytes);
        if !verify_ed25519(id_s_pub, &input, &self.sig) {
            return Err(DeviceCertError::SigVerify);
        }
        Ok(())
    }

    /// Verify the cert **and** that it names the identity that signed it.
    ///
    /// [`Self::verify`] alone answers "did `id_s_pub` sign this body?", which
    /// leaves one thing open: `body.identity_id` is a field like any other, so
    /// a holder of `ID_S_priv` can sign a body claiming to belong to somebody
    /// else's identity, and a reader that only checks the signature accepts it.
    /// The field is a *derivation* of `ID_S_pub`, so recomputing it is the
    /// whole check — and doing it in constant time keeps a comparison that
    /// runs on attacker-supplied bytes from leaking a prefix length.
    ///
    /// `expected_identity_id` is the identity this vault actually belongs to;
    /// passing it separately means a cert that verifies under a *different*
    /// well-formed identity is still refused, which is what makes
    /// `DeviceCertPublish` safe to self-authenticate.
    ///
    /// # Errors
    /// [`DeviceCertError::SigVerify`] on signature mismatch,
    /// [`DeviceCertError::IdentityMismatch`] when the cert names another
    /// identity.
    pub fn verify_binding(
        &self,
        id_s_pub: &[u8; 32],
        expected_identity_id: &[u8; 16],
    ) -> Result<(), DeviceCertError> {
        self.verify(id_s_pub)?;
        let derived = identity_id_from_pub(id_s_pub);
        let ok: bool = derived.ct_eq(&self.body.identity_id).into();
        let matches_expected: bool = derived.ct_eq(expected_identity_id).into();
        if !(ok && matches_expected) {
            return Err(DeviceCertError::IdentityMismatch);
        }
        Ok(())
    }

    /// Encode to canonical CBOR `{1: body_bytes, 2: sig}`.
    ///
    /// `body_bytes` is written **verbatim**, never re-encoded from
    /// [`Self::body`]. That is what makes `from_cbor(to_cbor(c)) == c` byte-for-
    /// byte for a cert that arrived from elsewhere, and it is why a relayed
    /// cert still verifies after this crate has round-tripped it.
    ///
    /// # Errors
    /// CBOR encode failure.
    pub fn to_cbor(&self) -> Result<Vec<u8>, DeviceCertError> {
        let map = vec![
            (
                Value::Integer(Integer::from(1)),
                Value::Bytes(self.body_bytes.clone()),
            ),
            (
                Value::Integer(Integer::from(2)),
                Value::Bytes(self.sig.to_vec()),
            ),
        ];
        let mut out = Vec::with_capacity(384);
        ciborium::ser::into_writer(&Value::Map(map), &mut out)
            .map_err(|e| DeviceCertError::Cbor(e.to_string()))?;
        Ok(out)
    }

    /// Parse a cert from its CBOR encoding `{1: body_bytes, 2: sig}`.
    ///
    /// This parses and does **not** verify: call [`Self::verify`] or
    /// [`Self::verify_binding`] before trusting the result. Map keys are
    /// accepted in any order, in the outer map and in the body, and integer
    /// keys this build does not know are skipped. `body_bytes` is kept as it
    /// arrived, so a non-canonical body still verifies against the bytes its
    /// signature covers. Bytes after the cert, or after the body inside
    /// `body_bytes`, are refused.
    ///
    /// # Errors
    /// [`DeviceCertError::Cbor`] when the input is not one CBOR item, has
    /// trailing bytes, is not a map, or has a non-integer key, and likewise
    /// for the body. [`DeviceCertError::BadField`] when a known field has the
    /// wrong type or length, or the body or signature or any of the eight body
    /// fields is missing. [`DeviceCertError::Validation`] when the nickname is
    /// empty or longer than [`MAX_NICKNAME_BYTES`] bytes.
    pub fn from_cbor(bytes: &[u8]) -> Result<Self, DeviceCertError> {
        // `sunrise_cbor::decode_value` reads **one** item and stops, so without
        // this a cert blob may carry anything at all after it — including
        // further whole certs. `roster_digest` hashes the blob it was handed,
        // so trailing bytes there were a way to present several signed certs as
        // one roster entry and satisfy the signed digest with a subset of the
        // membership. Nothing this workspace emits has a tail; refusing one is
        // free, and it makes "these bytes" and "this cert" the same thing.
        //
        // The nesting bound (`sunrise_cbor::MAX_VALUE_DEPTH`) applies to the
        // whole item, including a field this build skips: an ignored value is
        // still decoded into a tree before it is ignored.
        let mut cursor = bytes;
        let value = sunrise_cbor::decode_value(&mut cursor)
            .map_err(|e| DeviceCertError::Cbor(e.to_string()))?;
        if !cursor.is_empty() {
            return Err(DeviceCertError::Cbor(format!(
                "{} trailing byte(s) after the device cert",
                cursor.len()
            )));
        }
        let map = match value {
            Value::Map(m) => m,
            _ => return Err(DeviceCertError::Cbor("device cert must be a map".into())),
        };
        let mut body_bytes: Option<Vec<u8>> = None;
        let mut sig: Option<[u8; 64]> = None;
        for (k, v) in map {
            let id = match k {
                Value::Integer(i) => i128::from(i),
                _ => return Err(DeviceCertError::Cbor("non-int key".into())),
            };
            match (id, v) {
                // The body arrives as a byte string and is kept as one. No
                // re-encoding happens anywhere on this path, which is the point
                // of the shape: `verify` hashes exactly what is captured here.
                (1, Value::Bytes(b)) => body_bytes = Some(b),
                (2, Value::Bytes(b)) if b.len() == 64 => {
                    let mut a = [0u8; 64];
                    a.copy_from_slice(&b);
                    sig = Some(a);
                }
                (id, _) if (1..=2).contains(&id) => {
                    return Err(DeviceCertError::BadField("device cert outer"));
                }
                _ => {}
            }
        }
        let body_bytes = body_bytes.ok_or(DeviceCertError::BadField("body"))?;
        // Parsed for the caller's convenience, and parsed *after* the bytes are
        // captured. A parse failure is still a refusal: a cert whose body this
        // build cannot read is not one it can act on, whatever its signature
        // says.
        let body = body_from_cbor(&body_bytes)?;
        Ok(Self {
            body,
            sig: sig.ok_or(DeviceCertError::BadField("sig"))?,
            body_bytes,
        })
    }
}

#[cfg(test)]
mod tests;
