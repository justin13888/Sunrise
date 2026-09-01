//! `DeviceCert` — signed certificate binding a device to an identity.
//!
//! Per `docs/03-crypto/identity-and-device-keys.md`:
//!
//! ```cddl
//! DeviceCert = {
//!     body: {
//!         1: uint,                     ; v (= 1)
//!         2: bstr .size 16,            ; device_id
//!         3: bstr .size 32,            ; D_S_pub
//!         4: bstr .size 32,            ; D_D_pub
//!         5: bstr .size 16,            ; identity_id_bytes
//!         6: uint,                     ; created_at (ms since epoch)
//!         7: tstr .size (1..64),       ; nickname (utf-8)
//!         8: tstr,                     ; platform
//!     },
//!     sig: bstr .size 64               ; Ed25519_sign(ID_S_priv,
//!                                      ;   "sunrise.device_cert.v1" || BLAKE3(canonical_cbor(body), 32))
//! }
//! ```

use crate::identity::identity_id_from_pub;
use crate::keys::{verify_ed25519, IdentitySigningKeyPair};
use ciborium::value::{Integer, Value};
use subtle::ConstantTimeEq;
use thiserror::Error;

const DEVICE_CERT_DOMAIN: &[u8] = b"sunrise.device_cert.v1";

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
    /// Human-readable nickname (1..64 chars UTF-8).
    pub nickname: String,
    /// Platform string (e.g., "macos15", "ios18", "linux-x86_64").
    pub platform: String,
}

/// Outer device cert: inner body + Ed25519 signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCert {
    /// The inner body fields.
    pub body: DeviceCertInner,
    /// 64-byte Ed25519 signature over the domain prefix + BLAKE3 hash of the
    /// canonical-CBOR-encoded body.
    pub sig: [u8; 64],
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

fn body_to_cbor(body: &DeviceCertInner) -> Result<Vec<u8>, DeviceCertError> {
    if body.nickname.is_empty() || body.nickname.len() > 64 {
        return Err(DeviceCertError::Validation("nickname length"));
    }
    let map = vec![
        (
            Value::Integer(Integer::from(1)),
            Value::Integer(Integer::from(body.v)),
        ),
        (
            Value::Integer(Integer::from(2)),
            Value::Bytes(body.device_id.to_vec()),
        ),
        (
            Value::Integer(Integer::from(3)),
            Value::Bytes(body.d_s_pub.to_vec()),
        ),
        (
            Value::Integer(Integer::from(4)),
            Value::Bytes(body.d_d_pub.to_vec()),
        ),
        (
            Value::Integer(Integer::from(5)),
            Value::Bytes(body.identity_id.to_vec()),
        ),
        (
            Value::Integer(Integer::from(6)),
            Value::Integer(body.created_at_ms.into()),
        ),
        (
            Value::Integer(Integer::from(7)),
            Value::Text(body.nickname.clone()),
        ),
        (
            Value::Integer(Integer::from(8)),
            Value::Text(body.platform.clone()),
        ),
    ];
    let mut out = Vec::with_capacity(256);
    ciborium::ser::into_writer(&Value::Map(map), &mut out)
        .map_err(|e| DeviceCertError::Cbor(e.to_string()))?;
    Ok(out)
}

fn body_from_cbor(bytes: &[u8]) -> Result<DeviceCertInner, DeviceCertError> {
    let value: Value =
        ciborium::de::from_reader(bytes).map_err(|e| DeviceCertError::Cbor(e.to_string()))?;
    let map = match value {
        Value::Map(m) => m,
        _ => return Err(DeviceCertError::Cbor("body must be a map".into())),
    };
    let mut inner = DeviceCertInner {
        v: 0,
        device_id: [0u8; 16],
        d_s_pub: [0u8; 32],
        d_d_pub: [0u8; 32],
        identity_id: [0u8; 16],
        created_at_ms: 0,
        nickname: String::new(),
        platform: String::new(),
    };
    let mut got = [false; 8];
    for (k, v) in map {
        let id = match k {
            Value::Integer(i) => i128::from(i),
            _ => return Err(DeviceCertError::Cbor("non-int key".into())),
        };
        match (id, v) {
            (1, Value::Integer(i)) => {
                let raw: i128 = i128::from(i);
                inner.v = u32::try_from(raw).map_err(|_| DeviceCertError::BadField("v"))?;
                got[0] = true;
            }
            (2, Value::Bytes(b)) if b.len() == 16 => {
                inner.device_id.copy_from_slice(&b);
                got[1] = true;
            }
            (3, Value::Bytes(b)) if b.len() == 32 => {
                inner.d_s_pub.copy_from_slice(&b);
                got[2] = true;
            }
            (4, Value::Bytes(b)) if b.len() == 32 => {
                inner.d_d_pub.copy_from_slice(&b);
                got[3] = true;
            }
            (5, Value::Bytes(b)) if b.len() == 16 => {
                inner.identity_id.copy_from_slice(&b);
                got[4] = true;
            }
            (6, Value::Integer(i)) => {
                let raw: i128 = i128::from(i);
                inner.created_at_ms =
                    u64::try_from(raw).map_err(|_| DeviceCertError::BadField("created_at"))?;
                got[5] = true;
            }
            (7, Value::Text(s)) => {
                inner.nickname = s;
                got[6] = true;
            }
            (8, Value::Text(s)) => {
                inner.platform = s;
                got[7] = true;
            }
            (id, _) if (1..=8).contains(&id) => {
                return Err(DeviceCertError::BadField("body field shape"));
            }
            _ => {} // forward-compat
        }
    }
    if !got.iter().all(|x| *x) {
        return Err(DeviceCertError::BadField("missing body field"));
    }
    if inner.nickname.is_empty() || inner.nickname.len() > 64 {
        return Err(DeviceCertError::Validation("nickname length"));
    }
    Ok(inner)
}

fn sig_input_bytes(body: &DeviceCertInner) -> Result<Vec<u8>, DeviceCertError> {
    let body_cbor = body_to_cbor(body)?;
    let body_hash = blake3::hash(&body_cbor);
    let mut out = Vec::with_capacity(DEVICE_CERT_DOMAIN.len() + body_hash.as_bytes().len());
    out.extend_from_slice(DEVICE_CERT_DOMAIN);
    out.extend_from_slice(body_hash.as_bytes());
    Ok(out)
}

impl DeviceCert {
    /// Build a new device cert; sign the body with the identity signing key.
    ///
    /// # Errors
    /// CBOR / validation errors.
    pub fn issue(
        body: DeviceCertInner,
        identity_signing: &IdentitySigningKeyPair,
    ) -> Result<Self, DeviceCertError> {
        let input = sig_input_bytes(&body)?;
        let sig = identity_signing.sign(&input);
        Ok(Self { body, sig })
    }

    /// Verify the cert against the identity's `ID_S_pub`.
    ///
    /// # Errors
    /// Returns [`DeviceCertError::SigVerify`] on signature mismatch.
    pub fn verify(&self, id_s_pub: &[u8; 32]) -> Result<(), DeviceCertError> {
        let input = sig_input_bytes(&self.body)?;
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

    /// Encode to canonical CBOR `{ "body": ..., "sig": ... }` outer map.
    ///
    /// Map keys are integer ids 1 (body), 2 (sig).
    ///
    /// # Errors
    /// CBOR encode failure.
    pub fn to_cbor(&self) -> Result<Vec<u8>, DeviceCertError> {
        let body_cbor = body_to_cbor(&self.body)?;
        let body_value: Value = ciborium::de::from_reader(body_cbor.as_slice())
            .map_err(|e| DeviceCertError::Cbor(e.to_string()))?;
        let map = vec![
            (Value::Integer(Integer::from(1)), body_value),
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

    /// Decode from canonical CBOR.
    ///
    /// # Errors
    /// CBOR / field-shape failures.
    pub fn from_cbor(bytes: &[u8]) -> Result<Self, DeviceCertError> {
        let value: Value =
            ciborium::de::from_reader(bytes).map_err(|e| DeviceCertError::Cbor(e.to_string()))?;
        let map = match value {
            Value::Map(m) => m,
            _ => return Err(DeviceCertError::Cbor("device cert must be a map".into())),
        };
        let mut body: Option<DeviceCertInner> = None;
        let mut sig: Option<[u8; 64]> = None;
        for (k, v) in map {
            let id = match k {
                Value::Integer(i) => i128::from(i),
                _ => return Err(DeviceCertError::Cbor("non-int key".into())),
            };
            match (id, v) {
                (1, val) => {
                    // Re-encode body and re-decode for canonical-form check.
                    let mut buf = Vec::new();
                    ciborium::ser::into_writer(&val, &mut buf)
                        .map_err(|e| DeviceCertError::Cbor(e.to_string()))?;
                    body = Some(body_from_cbor(&buf)?);
                }
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
        Ok(Self {
            body: body.ok_or(DeviceCertError::BadField("body"))?,
            sig: sig.ok_or(DeviceCertError::BadField("sig"))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    fn fixture(d_s_pub: [u8; 32]) -> DeviceCertInner {
        DeviceCertInner {
            v: 1,
            device_id: [7u8; 16],
            d_s_pub,
            d_d_pub: [9u8; 32],
            identity_id: [3u8; 16],
            created_at_ms: 1_700_000_000_000,
            nickname: "Justin's Mac".to_string(),
            platform: "macos15".to_string(),
        }
    }

    #[test]
    fn issue_verify_round_trip() {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let identity = IdentitySigningKeyPair::generate(&mut rng);
        let device_signing = IdentitySigningKeyPair::generate(&mut rng);
        let body = fixture(device_signing.public_bytes());
        let cert = DeviceCert::issue(body, &identity).unwrap();
        cert.verify(&identity.public_bytes()).unwrap();
        // Wrong identity rejects.
        let other = IdentitySigningKeyPair::generate(&mut rng);
        assert!(matches!(
            cert.verify(&other.public_bytes()),
            Err(DeviceCertError::SigVerify)
        ));
    }

    #[test]
    fn cbor_round_trip() {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let identity = IdentitySigningKeyPair::generate(&mut rng);
        let body = fixture([1u8; 32]);
        let cert = DeviceCert::issue(body.clone(), &identity).unwrap();
        let bytes = cert.to_cbor().unwrap();
        let back = DeviceCert::from_cbor(&bytes).unwrap();
        assert_eq!(back, cert);
    }

    /// A cert can verify and still be a lie about which identity it belongs
    /// to: `identity_id` is a signed *field*, and only recomputing it from the
    /// signing key catches a rewrite.
    #[test]
    fn verify_binding_catches_a_rewritten_identity_id() {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let identity = IdentitySigningKeyPair::generate(&mut rng);
        let id_s_pub = identity.public_bytes();
        let real_id = crate::identity::identity_id_from_pub(&id_s_pub);

        let mut body = fixture([1u8; 32]);
        body.identity_id = real_id;
        let good = DeviceCert::issue(body.clone(), &identity).unwrap();
        good.verify_binding(&id_s_pub, &real_id).unwrap();

        // Same signer, a body claiming somebody else's identity.
        body.identity_id = [0xaa; 16];
        let liar = DeviceCert::issue(body, &identity).unwrap();
        liar.verify(&id_s_pub).expect("the signature is genuine");
        assert!(matches!(
            liar.verify_binding(&id_s_pub, &real_id),
            Err(DeviceCertError::IdentityMismatch)
        ));
    }

    /// A well-formed cert from a *different* identity is refused even though it
    /// verifies under its own key: the vault names the identity it trusts.
    #[test]
    fn verify_binding_refuses_another_identity() {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let ours = IdentitySigningKeyPair::generate(&mut rng);
        let theirs = IdentitySigningKeyPair::generate(&mut rng);
        let our_id = crate::identity::identity_id_from_pub(&ours.public_bytes());
        let their_pub = theirs.public_bytes();

        let mut body = fixture([1u8; 32]);
        body.identity_id = crate::identity::identity_id_from_pub(&their_pub);
        let cert = DeviceCert::issue(body, &theirs).unwrap();
        cert.verify_binding(&their_pub, &body_identity(&cert))
            .unwrap();
        assert!(matches!(
            cert.verify_binding(&their_pub, &our_id),
            Err(DeviceCertError::IdentityMismatch)
        ));
    }

    fn body_identity(cert: &DeviceCert) -> [u8; 16] {
        cert.body.identity_id
    }

    #[test]
    fn nickname_too_long_rejected() {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let identity = IdentitySigningKeyPair::generate(&mut rng);
        let mut body = fixture([1u8; 32]);
        body.nickname = "x".repeat(65);
        assert!(matches!(
            DeviceCert::issue(body, &identity),
            Err(DeviceCertError::Validation(_))
        ));
    }
}
