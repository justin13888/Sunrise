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
    /// Human-readable nickname, 1..=64 **bytes** of UTF-8 (not characters).
    ///
    /// The CDDL above says `tstr .size (1..64)`, and `.size` on a CDDL `tstr`
    /// bounds bytes. `sunrise-server` applies the same 64-byte *upper* bound to
    /// the `nickname` field of a device-registration request
    /// (`MAX_NICKNAME_BYTES`), so a character bound here would let a client
    /// build a cert naming a nickname its own registration call cannot carry.
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
                    // Re-encoded only so `body_from_cbor` can read it: the
                    // outer map has already been consumed into `Value`s, and
                    // the body decoder takes bytes. This is **not** a
                    // canonicity check — nothing here compares the re-encoding
                    // against the bytes that arrived, so a body with unsorted
                    // or non-minimal keys decodes exactly as a canonical one
                    // does. The comment used to claim otherwise. Canonical CBOR
                    // is unenforced across the whole inner-op surface (see the
                    // note in `docs/03-crypto/data-encryption-format.md`), and
                    // enforcing it here alone would reject certs this build's
                    // own encoder emits.
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

    // ---------------------------------------------------------------------
    // Field-width guards.
    //
    // The CDDL at the top of this file fixes **five** fields at an exact byte
    // width, and every one is enforced by a match guard on `b.len()`: four in
    // the body — `device_id` and `identity_id` at 16, `D_S_pub` and `D_D_pub`
    // at 32 — in `body_from_cbor`, and the 64-byte `sig` of the outer map in
    // `DeviceCert::from_cbor`. All five are pinned below.
    //
    // A guard that stopped guarding would not fail any test that only ever
    // feeds it well-formed certs, so the tests below feed it the one thing
    // that tells the two apart: a certificate identical to a good one except
    // that a single field is the wrong width.
    //
    // The bodies are built as `Value::Map`s rather than by editing encoded
    // bytes, so each test states the field it changed instead of a byte offset
    // that has to be decoded to be understood.
    // ---------------------------------------------------------------------

    /// The body fields of [`fixture`] as an editable list, **decoded from
    /// `body_to_cbor`'s own output** rather than retyped here.
    ///
    /// Retyping the encoder's key-id layout would let the two drift: renumber a
    /// field in production and these tests would keep sending the old id, which
    /// the in-range arm refuses as `BadField("body field shape")` — the very
    /// error the width tests assert, so all of them would pass while measuring
    /// nothing. Deriving the map means a renumbering moves the tests with it.
    fn body_fields() -> Vec<(i64, Value)> {
        let encoded = body_to_cbor(&fixture([1u8; 32])).expect("the fixture encodes");
        let decoded: Value =
            ciborium::de::from_reader(encoded.as_slice()).expect("its own output decodes");
        let Value::Map(map) = decoded else {
            panic!("body_to_cbor emits a map")
        };
        map.into_iter()
            .map(|(k, v)| match k {
                Value::Integer(i) => (
                    i64::try_from(i128::from(i)).expect("body field ids are small integers"),
                    v,
                ),
                other => panic!("body_to_cbor emits integer keys, got {other:?}"),
            })
            .collect()
    }

    fn encode_fields(fields: Vec<(i64, Value)>) -> Vec<u8> {
        let map = fields
            .into_iter()
            .map(|(id, v)| (Value::Integer(Integer::from(id)), v))
            .collect();
        let mut out = Vec::new();
        ciborium::ser::into_writer(&Value::Map(map), &mut out).expect("body map encodes");
        out
    }

    /// [`body_fields`] with the one field `id` carrying `value` instead.
    fn body_cbor_with(id: i64, value: Value) -> Vec<u8> {
        let mut fields = body_fields();
        let slot = fields
            .iter_mut()
            .find(|(k, _)| *k == id)
            .expect("the field id under test is one of the eight");
        slot.1 = value;
        encode_fields(fields)
    }

    /// Pins the ids **`body_to_cbor` emits**, and only those: [`body_fields`]
    /// is derived from the encoder, so this observes the encoding side alone.
    /// The decoder's own numbering is already pinned by the eight tests that
    /// decode a body and assert what came back.
    ///
    /// Deriving [`body_fields`] means a renumbering moves the tests with it —
    /// which is the point, but it also means no width test would notice one.
    /// The key ids are a wire contract (the CDDL at the top of this file), so
    /// pin them rather than leave the numbering unasserted in the crate.
    #[test]
    fn body_to_cbor_emits_exactly_the_eight_cddl_key_ids() {
        let ids: Vec<i64> = body_fields().into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            ids,
            vec![1, 2, 3, 4, 5, 6, 7, 8],
            "the device-cert body's key ids are a wire contract; renumbering one \
             breaks every peer that already speaks this format"
        );
    }

    /// Anchors every test below it: each wrong-width body differs from this one
    /// in exactly one field, so a refusal there is attributable to the width
    /// and to nothing else about how these bodies are built.
    #[test]
    fn the_unmodified_field_map_decodes_to_the_fixture() {
        let decoded = body_from_cbor(&encode_fields(body_fields())).expect("well-formed body");
        assert_eq!(decoded, fixture([1u8; 32]));
    }

    /// Wrong widths for a field the CDDL fixes at `width`: empty, one short,
    /// one long. Each must be refused by the guard rather than truncated,
    /// zero-extended, or (with the guard gone) panicked over inside
    /// `copy_from_slice`.
    fn assert_every_wrong_width_is_refused(id: i64, width: usize) {
        for n in [0, width - 1, width + 1] {
            let bytes = body_cbor_with(id, Value::Bytes(vec![0xab; n]));
            match body_from_cbor(&bytes) {
                Err(DeviceCertError::BadField("body field shape")) => {}
                other => panic!(
                    "body field {id} at {n} bytes (the CDDL fixes it at {width}): expected \
                     Err(BadField(\"body field shape\")), got {other:?}"
                ),
            }
        }
    }

    #[test]
    fn device_id_at_any_width_but_16_is_refused() {
        assert_every_wrong_width_is_refused(2, 16);
    }

    #[test]
    fn d_s_pub_at_any_width_but_32_is_refused() {
        assert_every_wrong_width_is_refused(3, 32);
    }

    #[test]
    fn d_d_pub_at_any_width_but_32_is_refused() {
        assert_every_wrong_width_is_refused(4, 32);
    }

    #[test]
    fn identity_id_at_any_width_but_16_is_refused() {
        assert_every_wrong_width_is_refused(5, 16);
    }

    /// A 32-byte value in a 16-byte field is the confusion a guard exists to
    /// stop: without it the decode would take the first 16 bytes of a public
    /// key and call them a device id.
    #[test]
    fn a_public_key_sized_value_is_not_a_device_id() {
        let bytes = body_cbor_with(2, Value::Bytes(vec![0xcd; 32]));
        assert!(matches!(
            body_from_cbor(&bytes),
            Err(DeviceCertError::BadField("body field shape"))
        ));
    }

    /// The outer `{1: body, 2: sig}` map [`DeviceCert::to_cbor`] emits, with
    /// the signature at `width` bytes instead of 64.
    fn outer_cbor_with_sig_of(width: usize) -> Vec<u8> {
        let encoded = body_to_cbor(&fixture([1u8; 32])).expect("the fixture encodes");
        let body: Value =
            ciborium::de::from_reader(encoded.as_slice()).expect("its own output decodes");
        let map = vec![
            (Value::Integer(Integer::from(1)), body),
            (
                Value::Integer(Integer::from(2)),
                Value::Bytes(vec![0xab; width]),
            ),
        ];
        let mut out = Vec::new();
        ciborium::ser::into_writer(&Value::Map(map), &mut out).expect("outer map encodes");
        out
    }

    /// The fifth length guard, and the only one on a **public** entry point.
    ///
    /// `sunrise-core` feeds `DeviceCert::from_cbor` certificates published by
    /// *other* devices on the `DeviceCertPublish` path, and that caller handles
    /// a refusal by warning and returning — so a guard that stopped guarding
    /// would not merely mis-parse, it would turn an expected `Err` into a
    /// `copy_from_slice` panic on the op-apply path.
    #[test]
    fn a_signature_at_any_width_but_64_is_refused() {
        // The control: at the right width this map shape decodes, so a refusal
        // below is the signature width and not the way the map is built.
        DeviceCert::from_cbor(&outer_cbor_with_sig_of(64)).expect("a 64-byte signature decodes");

        for n in [0, 63, 65] {
            match DeviceCert::from_cbor(&outer_cbor_with_sig_of(n)) {
                Err(DeviceCertError::BadField("device cert outer")) => {}
                other => panic!(
                    "a {n}-byte signature (the CDDL fixes it at 64): expected \
                     Err(BadField(\"device cert outer\")), got {other:?}"
                ),
            }
        }
    }

    // ---------------------------------------------------------------------
    // The nickname bound.
    //
    // `tstr .size (1..64)` makes 64 the longest admissible nickname, and both
    // `body_to_cbor` and `body_from_cbor` spell that as `len() > 64`. A
    // nickname of exactly 64 bytes is the single input where `>` and `>=`
    // disagree, so it is the only input that pins the comparison.
    // ---------------------------------------------------------------------

    #[test]
    fn a_nickname_of_exactly_64_bytes_encodes() {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let identity = IdentitySigningKeyPair::generate(&mut rng);
        let mut body = fixture([1u8; 32]);
        body.nickname = "x".repeat(64);
        assert_eq!(body.nickname.len(), 64, "the boundary this test exists for");

        let cert = DeviceCert::issue(body.clone(), &identity)
            .expect("64 bytes is inside the bound, not outside it");
        let back = DeviceCert::from_cbor(&cert.to_cbor().expect("encodes")).expect("decodes");
        assert_eq!(back.body.nickname, body.nickname);
        back.verify(&identity.public_bytes())
            .expect("a longest-legal nickname still verifies");
    }

    #[test]
    fn a_nickname_of_exactly_64_bytes_decodes() {
        let nickname = "x".repeat(64);
        let bytes = body_cbor_with(7, Value::Text(nickname.clone()));
        assert_eq!(
            body_from_cbor(&bytes)
                .expect("64 bytes is inside the bound")
                .nickname,
            nickname
        );
    }

    #[test]
    fn a_nickname_of_65_bytes_is_refused_by_the_decoder() {
        let bytes = body_cbor_with(7, Value::Text("x".repeat(65)));
        assert!(matches!(
            body_from_cbor(&bytes),
            Err(DeviceCertError::Validation("nickname length"))
        ));
    }

    // The bound is on BYTES, not characters. With ASCII nicknames the two
    // readings are indistinguishable, so `"x".repeat(n)` alone leaves
    // `len()` silently reinterpretable as `chars().count()`. A three-byte
    // character separates them: 22 of them are 66 bytes but only 22
    // characters, so the byte reading refuses what the character reading
    // waves through. Getting this wrong is not academic — `sunrise-server`
    // refuses a nickname over 64 *bytes*
    // (`crates/sunrise-server/src/api/devices.rs`, `MAX_NICKNAME_BYTES`), so a
    // character bound here would mint a validly signed cert on the pairing
    // path that the service then rejects.
    /// U+65E5, three bytes in UTF-8.
    const MULTIBYTE: &str = "\u{65e5}";

    #[test]
    fn a_multibyte_nickname_of_63_bytes_is_accepted() {
        let nickname = MULTIBYTE.repeat(21);
        assert_eq!((nickname.len(), nickname.chars().count()), (63, 21));

        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let identity = IdentitySigningKeyPair::generate(&mut rng);
        let mut body = fixture([1u8; 32]);
        body.nickname = nickname.clone();
        let cert = DeviceCert::issue(body, &identity).expect("63 bytes is inside the bound");
        let back = DeviceCert::from_cbor(&cert.to_cbor().expect("encodes")).expect("decodes");
        assert_eq!(back.body.nickname, nickname);

        assert_eq!(
            body_from_cbor(&body_cbor_with(7, Value::Text(nickname.clone())))
                .expect("63 bytes is inside the bound")
                .nickname,
            nickname
        );
    }

    /// The falsifier for the unit: 66 bytes but only 22 characters, so this is
    /// refused under the byte bound the CDDL and `sunrise-server` both state,
    /// and accepted under a character bound.
    #[test]
    fn a_multibyte_nickname_of_66_bytes_is_refused_though_it_is_only_22_characters() {
        let nickname = MULTIBYTE.repeat(22);
        assert_eq!((nickname.len(), nickname.chars().count()), (66, 22));
        assert!(
            nickname.chars().count() <= 64,
            "the character count must stay inside the bound, or this proves nothing"
        );

        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let identity = IdentitySigningKeyPair::generate(&mut rng);
        let mut body = fixture([1u8; 32]);
        body.nickname = nickname.clone();
        assert!(
            matches!(
                DeviceCert::issue(body, &identity),
                Err(DeviceCertError::Validation("nickname length"))
            ),
            "body_to_cbor must bound the nickname in bytes, not characters"
        );

        assert!(
            matches!(
                body_from_cbor(&body_cbor_with(7, Value::Text(nickname))),
                Err(DeviceCertError::Validation("nickname length"))
            ),
            "body_from_cbor must bound the nickname in bytes, not characters"
        );
    }

    /// The encoder half of the bound's lower end. Without it `DeviceCert::issue`
    /// mints and signs a cert that its own decoder refuses — a valid-looking
    /// artefact no peer can read, discovered only at read time on the pairing
    /// path.
    #[test]
    fn an_empty_nickname_is_refused_by_the_encoder() {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let identity = IdentitySigningKeyPair::generate(&mut rng);
        let mut body = fixture([1u8; 32]);
        body.nickname = String::new();
        assert!(matches!(
            DeviceCert::issue(body, &identity),
            Err(DeviceCertError::Validation("nickname length"))
        ));
    }

    #[test]
    fn an_empty_nickname_is_refused_by_the_decoder() {
        let bytes = body_cbor_with(7, Value::Text(String::new()));
        assert!(matches!(
            body_from_cbor(&bytes),
            Err(DeviceCertError::Validation("nickname length"))
        ));
    }
}
