//! The account's key bundle: every current Stream key, sealed to the account
//! identity and signed by the device that published it (ADR-0069).
//!
//! A device restored from the recovery code holds the identity and nothing
//! else. Every Stream key reaches it as an identity-addressed `key_envelope`
//! op, and the relay trims ops (30 days or 256 MiB per channel). A bundle is
//! the same set of keys in one structure the relay pins outside retention, so
//! a recovery does not depend on how old the account is.
//!
//! # Wire shape
//!
//! ```text
//! bundle = "SR" 0x08 version(u16 BE = 1) || canonical CBOR map
//! ```
//!
//! | Key | Field | |
//! |---|---|---|
//! | 1 | `identity_id` | `bstr .size 16`, the identity the bundle is sealed to |
//! | 2 | `signer_cert` | the publishing device's identity-signed `DeviceCert` |
//! | 3 | `generated_at_ms` | the publisher's wall clock; informational only |
//! | 4 | `manifest` | `[* [stream_id, epoch, n_keys]]`, sorted, unique |
//! | 5 | `sealed` | HPKE `enc ‖ ct ‖ tag` of the canonical key list |
//! | 6 | `sig` | Ed25519 by the publisher's `D_S` |
//!
//! The plaintext is `[* [stream_id, epoch, key]]`, sorted and unique. HPKE is
//! Base mode to `ID_D_pub` with `info = "sunrise.key_bundle.hpke.v1" ‖
//! identity_id` and `aad = magic ‖ canonical({1..4})`. The signature covers
//! `"sunrise.key_bundle.sig.v1" ‖ BLAKE3(magic ‖ canonical({1..5}))`.
//!
//! # What binds what
//!
//! - **The recipient.** `identity_id` is in the HPKE `info`, so a bundle
//!   sealed to one identity does not open as another's, and a reader refuses a
//!   header naming any identity but its own before it opens anything.
//! - **The publisher.** The signature is by a device whose cert verifies under
//!   the reader's identity. A relay holds no device key, so it cannot write a
//!   bundle, alter one, or move one between accounts.
//! - **The header and the keys together.** The header is the HPKE AAD, so a
//!   ciphertext moved under another header fails to open even where its
//!   signature has been made to verify.
//! - **The manifest and the keys.** The opened key list must match the
//!   manifest entry for entry. The manifest is cleartext so a device that holds
//!   no `ID_D_priv` (every paired one) can tell whether the bundle the relay
//!   pins already holds every key it holds; it names streams and epochs, which
//!   the relay already reads off every op envelope's header.
//!
//! # What does not bind
//!
//! **Freshness.** A reader with nothing but the identity cannot tell the
//! newest bundle from an older one its account also signed: both verify. The
//! relay can therefore serve an old bundle. ADR-0069 §Rollback states what that
//! costs and why it is no more than withholding ops already costs.

use crate::blake3_kdf::derive_key_32;
use crate::device_cert::DeviceCert;
use crate::hpke_seal::{hpke_open_identity, hpke_seal, HpkeError};
use crate::keys::{verify_ed25519, IdentityDhKeyPair, StreamKey};
use ciborium::value::{Integer, Value};
use rand_core::CryptoRngCore;
use subtle::ConstantTimeEq;
use sunrise_cbor::magic::{decode_prefix, write_prefix, MagicKind, MagicPrefix, MAGIC_LEN};
use thiserror::Error;
use zeroize::Zeroize;

/// The key bundle's format version, carried in its magic prefix.
pub const KEY_BUNDLE_FORMAT_V: u16 = 1;

/// Domain prefix of the HPKE `info`, followed by the recipient `identity_id`.
const HPKE_INFO_PREFIX: &[u8] = b"sunrise.key_bundle.hpke.v1";
/// Domain prefix of the publisher's signature input.
const SIG_DOMAIN: &[u8] = b"sunrise.key_bundle.sig.v1";
/// `derive_key` context of [`manifest_digest`].
const MANIFEST_CONTEXT: &str = "sunrise.key_bundle.manifest.v1";

/// The most keys one bundle may carry. Far above any account (a stream holds
/// one key per rotation), and low enough that a hostile bundle cannot make a
/// reader allocate without bound.
pub const MAX_BUNDLE_KEYS: usize = 1 << 16;

/// The largest bundle a reader decodes: 64 bytes a key with room, at
/// [`MAX_BUNDLE_KEYS`].
pub const MAX_BUNDLE_BYTES: usize = 8 * 1024 * 1024;

/// The longest `signer_cert` accepted. A cert is about three hundred bytes.
const MAX_CERT_BYTES: usize = 4096;

/// One Stream key in a bundle.
#[derive(Clone, PartialEq, Eq)]
pub struct BundledKey {
    /// The stream it opens.
    pub stream_id: [u8; 16],
    /// Its epoch.
    pub epoch: u32,
    /// The key.
    pub key: StreamKey,
}

impl std::fmt::Debug for BundledKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BundledKey")
            .field("stream_id", &self.stream_id)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

/// One manifest line: how many keys the bundle carries for a `(stream,
/// epoch)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManifestEntry {
    /// The stream.
    pub stream_id: [u8; 16],
    /// The epoch.
    pub epoch: u32,
    /// How many distinct keys are held at that epoch: one, except where two
    /// devices minted the same epoch concurrently.
    pub keys: u32,
}

/// Key bundle errors. One variant per thing a caller does differently.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum KeyBundleError {
    /// Not a key bundle at all: the magic prefix, the version, the CBOR, a
    /// field's shape, or a bound.
    #[error("not a key bundle: {0}")]
    Malformed(&'static str),
    /// A well-formed bundle sealed to another identity. A relay that serves
    /// one is serving another account's bundle.
    #[error("the key bundle is sealed to another identity")]
    OtherIdentity,
    /// The publisher's cert does not verify under this identity.
    #[error("the key bundle's publisher is not a device of this account")]
    ForeignSigner,
    /// The signature does not verify under the publisher's cert.
    #[error("the key bundle's signature does not verify")]
    BadSignature,
    /// The keys did not open, or opened to something other than the manifest.
    #[error("the key bundle's keys do not open under this identity")]
    Open,
}

impl From<HpkeError> for KeyBundleError {
    fn from(_: HpkeError) -> Self {
        Self::Open
    }
}

/// The manifest of `keys`: one entry per `(stream, epoch)`, sorted.
#[must_use]
pub fn manifest_of(keys: &[BundledKey]) -> Vec<ManifestEntry> {
    let mut counts: std::collections::BTreeMap<([u8; 16], u32), u32> =
        std::collections::BTreeMap::new();
    for k in keys {
        let n = counts.entry((k.stream_id, k.epoch)).or_insert(0);
        *n = n.saturating_add(1);
    }
    counts
        .into_iter()
        .map(|((stream_id, epoch), keys)| ManifestEntry {
            stream_id,
            epoch,
            keys,
        })
        .collect()
}

/// A digest of a manifest, which the relay stores beside the pin so a device
/// can tell an unchanged bundle without fetching it.
///
/// The relay cannot check it against the bundle, and nothing trusts it: a
/// device that finds it equal to its own does nothing, and a relay that lies
/// about it can only make a device skip a publish it could also refuse.
#[must_use]
pub fn manifest_digest(manifest: &[ManifestEntry]) -> [u8; 32] {
    // Encoding a list of byte strings and integers cannot fail.
    let bytes = sunrise_cbor::encode_canonical(&manifest_value(manifest)).unwrap_or_default();
    derive_key_32(MANIFEST_CONTEXT, &bytes)
}

/// Whether a bundle with manifest `held` carries at least every key `other`
/// names.
#[must_use]
pub fn manifest_covers(held: &[ManifestEntry], other: &[ManifestEntry]) -> bool {
    other.iter().all(|o| {
        held.iter()
            .any(|h| h.stream_id == o.stream_id && h.epoch == o.epoch && h.keys >= o.keys)
    })
}

fn int(n: u64) -> Value {
    Value::Integer(Integer::from(n))
}

fn manifest_value(manifest: &[ManifestEntry]) -> Value {
    Value::Array(
        manifest
            .iter()
            .map(|e| {
                Value::Array(vec![
                    Value::Bytes(e.stream_id.to_vec()),
                    int(u64::from(e.epoch)),
                    int(u64::from(e.keys)),
                ])
            })
            .collect(),
    )
}

fn prefix() -> [u8; MAGIC_LEN] {
    let mut p = [0u8; MAGIC_LEN];
    write_prefix(&mut p, MagicKind::KeyBundle, KEY_BUNDLE_FORMAT_V);
    p
}

fn hpke_info(identity_id: &[u8; 16]) -> Vec<u8> {
    let mut info = Vec::with_capacity(HPKE_INFO_PREFIX.len() + 16);
    info.extend_from_slice(HPKE_INFO_PREFIX);
    info.extend_from_slice(identity_id);
    info
}

fn encode(v: &Value) -> Result<Vec<u8>, KeyBundleError> {
    sunrise_cbor::encode_canonical(v).map_err(|_| KeyBundleError::Malformed("encode"))
}

/// `magic ‖ canonical(fields)`: the HPKE AAD over fields 1 to 4, and the
/// signed bytes over fields 1 to 5.
fn framed(fields: &[(u64, Value)]) -> Result<Vec<u8>, KeyBundleError> {
    let map = Value::Map(fields.iter().map(|(k, v)| (int(*k), v.clone())).collect());
    let mut out = prefix().to_vec();
    out.extend_from_slice(&encode(&map)?);
    Ok(out)
}

fn sig_input(signed: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(SIG_DOMAIN.len() + 32);
    out.extend_from_slice(SIG_DOMAIN);
    out.extend_from_slice(blake3::hash(signed).as_bytes());
    out
}

fn header_fields(header: &KeyBundleHeader) -> Vec<(u64, Value)> {
    vec![
        (1, Value::Bytes(header.identity_id.to_vec())),
        (2, Value::Bytes(header.signer_cert.clone())),
        (3, int(header.generated_at_ms)),
        (4, manifest_value(&header.manifest)),
    ]
}

/// Seal `keys` to the identity `(identity_id, id_d_pub)` and sign the result
/// with `sign`, the publishing device's `D_S`.
///
/// `signer_cert` is that device's cert; a reader verifies it under the
/// identity and the signature under it.
///
/// # Errors
/// [`KeyBundleError::Malformed`] past [`MAX_BUNDLE_KEYS`] or for a cert past
/// its bound, and [`KeyBundleError::Open`] for a recipient key that is not a
/// key.
pub fn seal_key_bundle<R: CryptoRngCore>(
    identity_id: &[u8; 16],
    id_d_pub: &[u8; 32],
    signer_cert: &[u8],
    generated_at_ms: u64,
    keys: &[BundledKey],
    sign: &dyn Fn(&[u8]) -> [u8; 64],
    rng: &mut R,
) -> Result<Vec<u8>, KeyBundleError> {
    if keys.len() > MAX_BUNDLE_KEYS {
        return Err(KeyBundleError::Malformed("too many keys"));
    }
    if signer_cert.len() > MAX_CERT_BYTES {
        return Err(KeyBundleError::Malformed("signer cert too long"));
    }
    let mut sorted: Vec<&BundledKey> = keys.iter().collect();
    sorted.sort_by(|a, b| {
        (a.stream_id, a.epoch, a.key.as_bytes()).cmp(&(b.stream_id, b.epoch, b.key.as_bytes()))
    });
    sorted.dedup_by(|a, b| a == b);
    let owned: Vec<BundledKey> = sorted.into_iter().cloned().collect();
    let header = KeyBundleHeader {
        identity_id: *identity_id,
        signer_cert: signer_cert.to_vec(),
        generated_at_ms,
        manifest: manifest_of(&owned),
    };
    let mut fields = header_fields(&header);
    let aad = framed(&fields)?;

    let mut plain = encode(&Value::Array(
        owned
            .iter()
            .map(|k| {
                Value::Array(vec![
                    Value::Bytes(k.stream_id.to_vec()),
                    int(u64::from(k.epoch)),
                    Value::Bytes(k.key.as_bytes().to_vec()),
                ])
            })
            .collect(),
    ))?;
    let sealed = hpke_seal(id_d_pub, &hpke_info(identity_id), &plain, &aad, rng);
    plain.zeroize();
    let sealed = sealed?;

    fields.push((5, Value::Bytes(sealed)));
    let signed = framed(&fields)?;
    let sig = sign(&sig_input(&signed));
    fields.push((6, Value::Bytes(sig.to_vec())));
    framed(&fields)
}

/// The cleartext half of a bundle: what anyone holding it can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBundleHeader {
    /// The identity the keys are sealed to.
    pub identity_id: [u8; 16],
    /// The publishing device's cert, as carried.
    pub signer_cert: Vec<u8>,
    /// The publisher's wall clock when it sealed the bundle. Informational:
    /// nothing orders bundles by it, for ADR-0068 §2's reason.
    pub generated_at_ms: u64,
    /// One entry per `(stream, epoch)` the bundle carries keys for.
    pub manifest: Vec<ManifestEntry>,
}

/// A decoded, not yet verified, key bundle.
#[derive(Debug, Clone)]
pub struct KeyBundle {
    header: KeyBundleHeader,
    sealed: Vec<u8>,
    sig: [u8; 64],
}

fn fixed<const N: usize>(v: &Value) -> Option<[u8; N]> {
    v.as_bytes()?.as_slice().try_into().ok()
}

fn as_u32(v: &Value) -> Option<u32> {
    u32::try_from(v.as_integer()?).ok()
}

fn as_u64(v: &Value) -> Option<u64> {
    u64::try_from(v.as_integer()?).ok()
}

fn decode_manifest(v: &Value) -> Option<Vec<ManifestEntry>> {
    let items = v.as_array()?;
    if items.len() > MAX_BUNDLE_KEYS {
        return None;
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let [s, e, n] = item.as_array()?.as_slice() else {
            return None;
        };
        let entry = ManifestEntry {
            stream_id: fixed(s)?,
            epoch: as_u32(e)?,
            keys: as_u32(n)?,
        };
        if entry.keys < 1 {
            return None;
        }
        out.push(entry);
    }
    // Sorted and unique, so one bundle has one manifest and one digest.
    if out
        .windows(2)
        .any(|w| (w[0].stream_id, w[0].epoch) >= (w[1].stream_id, w[1].epoch))
    {
        return None;
    }
    Some(out)
}

/// Decode a bundle: the magic prefix, the format version, canonical CBOR, and
/// every field's shape and bound. Nothing is verified yet.
///
/// # Errors
/// [`KeyBundleError::Malformed`].
pub fn decode_key_bundle(bytes: &[u8]) -> Result<KeyBundle, KeyBundleError> {
    if bytes.len() > MAX_BUNDLE_BYTES {
        return Err(KeyBundleError::Malformed("too large"));
    }
    if !matches!(
        decode_prefix(bytes),
        Ok(MagicPrefix {
            kind: MagicKind::KeyBundle,
            version: KEY_BUNDLE_FORMAT_V,
        })
    ) {
        return Err(KeyBundleError::Malformed("magic prefix"));
    }
    let value: Value = sunrise_cbor::decode_canonical(&bytes[MAGIC_LEN..])
        .map_err(|_| KeyBundleError::Malformed("not canonical CBOR"))?;
    let Value::Map(entries) = value else {
        return Err(KeyBundleError::Malformed("not a map"));
    };
    let mut fields: [Option<Value>; 6] = Default::default();
    for (k, v) in entries {
        let slot = k
            .as_integer()
            .and_then(|i| usize::try_from(i).ok())
            .filter(|i| (1..=6).contains(i))
            .ok_or(KeyBundleError::Malformed("unknown field"))?;
        if fields[slot - 1].replace(v).is_some() {
            return Err(KeyBundleError::Malformed("duplicate field"));
        }
    }
    let field = |i: usize| {
        fields[i - 1]
            .as_ref()
            .ok_or(KeyBundleError::Malformed("missing field"))
    };
    let identity_id = fixed(field(1)?).ok_or(KeyBundleError::Malformed("identity_id"))?;
    let signer_cert = field(2)?
        .as_bytes()
        .filter(|c| c.len() <= MAX_CERT_BYTES)
        .ok_or(KeyBundleError::Malformed("signer_cert"))?
        .clone();
    let generated_at_ms = as_u64(field(3)?).ok_or(KeyBundleError::Malformed("generated_at"))?;
    let manifest = decode_manifest(field(4)?).ok_or(KeyBundleError::Malformed("manifest"))?;
    let sealed = field(5)?
        .as_bytes()
        .ok_or(KeyBundleError::Malformed("sealed"))?
        .clone();
    let sig = fixed(field(6)?).ok_or(KeyBundleError::Malformed("sig"))?;
    Ok(KeyBundle {
        header: KeyBundleHeader {
            identity_id,
            signer_cert,
            generated_at_ms,
            manifest,
        },
        sealed,
        sig,
    })
}

impl KeyBundle {
    /// The cleartext half.
    #[must_use]
    pub fn header(&self) -> &KeyBundleHeader {
        &self.header
    }

    /// Check that this bundle is sealed to `identity_id`, that its publisher
    /// is a device that identity certified, and that the publisher signed it.
    ///
    /// The identity is checked first, and by itself: a relay serving another
    /// account's bundle is told apart from a forgery.
    ///
    /// # Errors
    /// [`KeyBundleError::OtherIdentity`], [`KeyBundleError::ForeignSigner`],
    /// [`KeyBundleError::BadSignature`].
    pub fn verify(
        &self,
        identity_id: &[u8; 16],
        id_s_pub: &[u8; 32],
    ) -> Result<VerifiedKeyBundle<'_>, KeyBundleError> {
        let same: bool = self.header.identity_id.ct_eq(identity_id).into();
        if !same {
            return Err(KeyBundleError::OtherIdentity);
        }
        let cert = DeviceCert::from_cbor(&self.header.signer_cert)
            .map_err(|_| KeyBundleError::ForeignSigner)?;
        cert.verify_binding(id_s_pub, identity_id)
            .map_err(|_| KeyBundleError::ForeignSigner)?;
        let mut fields = header_fields(&self.header);
        fields.push((5, Value::Bytes(self.sealed.clone())));
        let signed = framed(&fields)?;
        if !verify_ed25519(&cert.body.d_s_pub, &sig_input(&signed), &self.sig) {
            return Err(KeyBundleError::BadSignature);
        }
        Ok(VerifiedKeyBundle {
            bundle: self,
            signer: cert,
        })
    }
}

/// A bundle whose recipient, publisher and signature have been checked.
#[derive(Debug)]
pub struct VerifiedKeyBundle<'a> {
    bundle: &'a KeyBundle,
    signer: DeviceCert,
}

impl VerifiedKeyBundle<'_> {
    /// The cleartext half.
    #[must_use]
    pub fn header(&self) -> &KeyBundleHeader {
        &self.bundle.header
    }

    /// The publishing device's cert, verified under the identity.
    #[must_use]
    pub fn signer(&self) -> &DeviceCert {
        &self.signer
    }

    /// Open the keys with the identity's `ID_D_priv`.
    ///
    /// The list must be sorted, unique, and exactly what the manifest says.
    ///
    /// # Errors
    /// [`KeyBundleError::Open`] for a wrong key, a moved ciphertext, or a key
    /// list that disagrees with its manifest.
    pub fn open(&self, id_d: &IdentityDhKeyPair) -> Result<Vec<BundledKey>, KeyBundleError> {
        let header = &self.bundle.header;
        let aad = framed(&header_fields(header))?;
        let mut plain = hpke_open_identity(
            id_d,
            &hpke_info(&header.identity_id),
            &self.bundle.sealed,
            &aad,
        )?;
        let decoded: Result<Value, _> = sunrise_cbor::decode_canonical(&plain);
        plain.zeroize();
        let Ok(Value::Array(items)) = decoded else {
            return Err(KeyBundleError::Open);
        };
        if items.len() > MAX_BUNDLE_KEYS {
            return Err(KeyBundleError::Open);
        }
        let mut keys = Vec::with_capacity(items.len());
        for item in items {
            let Value::Array(parts) = item else {
                return Err(KeyBundleError::Open);
            };
            let [s, e, k] = parts.as_slice() else {
                return Err(KeyBundleError::Open);
            };
            let (Some(stream_id), Some(epoch), Some(mut bytes)) =
                (fixed::<16>(s), as_u32(e), fixed::<32>(k))
            else {
                return Err(KeyBundleError::Open);
            };
            keys.push(BundledKey {
                stream_id,
                epoch,
                key: StreamKey::from_bytes(bytes),
            });
            bytes.zeroize();
        }
        let ordered = keys.windows(2).all(|w| {
            (w[0].stream_id, w[0].epoch, w[0].key.as_bytes())
                < (w[1].stream_id, w[1].epoch, w[1].key.as_bytes())
        });
        if !ordered || manifest_of(&keys) != header.manifest {
            return Err(KeyBundleError::Open);
        }
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_cert::DeviceCertInner;
    use crate::identity::identity_id_from_pub;
    use crate::keys::{DeviceSigningKeyPair, IdentitySigningKeyPair};
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    struct Account {
        id_s: IdentitySigningKeyPair,
        id_d: IdentityDhKeyPair,
        identity_id: [u8; 16],
    }

    fn account(seed: u8) -> Account {
        let id_s = IdentitySigningKeyPair::from_secret_bytes(&[seed; 32]);
        let id_d = IdentityDhKeyPair::from_secret_bytes([seed.wrapping_add(1); 32]);
        let identity_id = identity_id_from_pub(&id_s.public_bytes());
        Account {
            id_s,
            id_d,
            identity_id,
        }
    }

    /// A device of `acct`: its signing key and the cert `acct` issued it.
    fn device(acct: &Account, seed: u8) -> (DeviceSigningKeyPair, Vec<u8>) {
        let d_s = DeviceSigningKeyPair::from_secret_bytes(&[seed; 32]);
        let cert = DeviceCert::issue(
            DeviceCertInner {
                v: 1,
                device_id: [seed; 16],
                d_s_pub: d_s.public_bytes(),
                d_d_pub: [seed; 32],
                identity_id: acct.identity_id,
                created_at_ms: 1,
                nickname: "laptop".into(),
                platform: "linux".into(),
            },
            &acct.id_s,
        )
        .unwrap()
        .to_cbor()
        .unwrap();
        (d_s, cert)
    }

    fn keys() -> Vec<BundledKey> {
        vec![
            BundledKey {
                stream_id: [0; 16],
                epoch: 2,
                key: StreamKey::from_bytes([0x22; 32]),
            },
            BundledKey {
                stream_id: [0; 16],
                epoch: 1,
                key: StreamKey::from_bytes([0x11; 32]),
            },
            BundledKey {
                stream_id: [7; 16],
                epoch: 1,
                key: StreamKey::from_bytes([0x71; 32]),
            },
        ]
    }

    fn seal(acct: &Account, d_s: &DeviceSigningKeyPair, cert: &[u8], k: &[BundledKey]) -> Vec<u8> {
        let mut rng = ChaCha20Rng::seed_from_u64(9);
        seal_key_bundle(
            &acct.identity_id,
            &acct.id_d.public_bytes(),
            cert,
            1_000,
            k,
            &|m| d_s.sign(m),
            &mut rng,
        )
        .unwrap()
    }

    fn sorted(mut k: Vec<BundledKey>) -> Vec<BundledKey> {
        k.sort_by(|a, b| (a.stream_id, a.epoch).cmp(&(b.stream_id, b.epoch)));
        k
    }

    #[test]
    fn a_bundle_round_trips_to_its_identity() {
        let acct = account(1);
        let (d_s, cert) = device(&acct, 5);
        let bytes = seal(&acct, &d_s, &cert, &keys());
        assert_eq!(&bytes[..3], b"SR\x08");
        let bundle = decode_key_bundle(&bytes).unwrap();
        let verified = bundle
            .verify(&acct.identity_id, &acct.id_s.public_bytes())
            .unwrap();
        assert_eq!(verified.signer().body.device_id, [5; 16]);
        assert_eq!(verified.open(&acct.id_d).unwrap(), sorted(keys()));
        assert_eq!(
            bundle.header().manifest,
            vec![
                ManifestEntry {
                    stream_id: [0; 16],
                    epoch: 1,
                    keys: 1
                },
                ManifestEntry {
                    stream_id: [0; 16],
                    epoch: 2,
                    keys: 1
                },
                ManifestEntry {
                    stream_id: [7; 16],
                    epoch: 1,
                    keys: 1
                },
            ]
        );
    }

    /// Substitution: a relay that serves account B's bundle to account A is
    /// refused on the identity before anything else is read, and B's keys do
    /// not open under A's `ID_D_priv` even if the check were skipped.
    #[test]
    fn another_accounts_bundle_is_refused() {
        let a = account(1);
        let b = account(3);
        let (d_s, cert) = device(&b, 5);
        let bytes = seal(&b, &d_s, &cert, &keys());
        let bundle = decode_key_bundle(&bytes).unwrap();
        assert_eq!(
            bundle
                .verify(&a.identity_id, &a.id_s.public_bytes())
                .unwrap_err(),
            KeyBundleError::OtherIdentity
        );
        let verified = bundle
            .verify(&b.identity_id, &b.id_s.public_bytes())
            .unwrap();
        assert_eq!(verified.open(&a.id_d).unwrap_err(), KeyBundleError::Open);
    }

    /// A bundle claiming account A's identity, signed by a device another
    /// identity certified, is refused: only A's own devices publish for A.
    #[test]
    fn a_publisher_another_identity_certified_is_refused() {
        let a = account(1);
        let b = account(3);
        let (d_s, cert) = device(&b, 5);
        let bytes = seal(&a, &d_s, &cert, &keys());
        let bundle = decode_key_bundle(&bytes).unwrap();
        assert_eq!(
            bundle
                .verify(&a.identity_id, &a.id_s.public_bytes())
                .unwrap_err(),
            KeyBundleError::ForeignSigner
        );
    }

    /// The relay holds no device key: any byte it changes in the header or
    /// the ciphertext fails the signature.
    #[test]
    fn a_relay_cannot_alter_a_bundle() {
        let acct = account(1);
        let (d_s, cert) = device(&acct, 5);
        let bytes = seal(&acct, &d_s, &cert, &keys());
        let mut refused = 0;
        for i in MAGIC_LEN..bytes.len() {
            let mut t = bytes.clone();
            t[i] ^= 0x01;
            let Ok(bundle) = decode_key_bundle(&t) else {
                refused += 1;
                continue;
            };
            let opened = bundle
                .verify(&acct.identity_id, &acct.id_s.public_bytes())
                .and_then(|v| v.open(&acct.id_d));
            assert!(
                opened.is_err(),
                "byte {i} flipped and the bundle still opened"
            );
            refused += 1;
        }
        assert_eq!(refused, bytes.len() - MAGIC_LEN);
    }

    /// The header is the HPKE AAD: a device that re-signs one bundle's
    /// ciphertext under another bundle's header still cannot open it.
    #[test]
    fn a_ciphertext_moved_under_another_header_does_not_open() {
        let acct = account(1);
        let (d_s, cert) = device(&acct, 5);
        let old = decode_key_bundle(&seal(&acct, &d_s, &cert, &keys()[..1])).unwrap();
        let new = decode_key_bundle(&seal(&acct, &d_s, &cert, &keys())).unwrap();
        // Splice: the new header, the old keys, re-signed by a real device.
        let mut fields = header_fields(new.header());
        fields.push((5, Value::Bytes(old.sealed.clone())));
        let sig = d_s.sign(&sig_input(&framed(&fields).unwrap()));
        fields.push((6, Value::Bytes(sig.to_vec())));
        let spliced = decode_key_bundle(&framed(&fields).unwrap()).unwrap();
        let verified = spliced
            .verify(&acct.identity_id, &acct.id_s.public_bytes())
            .unwrap();
        assert_eq!(verified.open(&acct.id_d).unwrap_err(), KeyBundleError::Open);
    }

    /// Rollback: an older bundle of the same account verifies — freshness is
    /// not something a reader holding only the identity can check, and the
    /// ADR says so — and its manifest shows what it lacks, which is what a
    /// publisher uses to refuse to pin it over a newer one.
    #[test]
    fn an_older_bundle_verifies_and_its_manifest_shows_what_it_lacks() {
        let acct = account(1);
        let (d_s, cert) = device(&acct, 5);
        let old = decode_key_bundle(&seal(&acct, &d_s, &cert, &keys()[1..])).unwrap();
        let new = decode_key_bundle(&seal(&acct, &d_s, &cert, &keys())).unwrap();
        assert!(old
            .verify(&acct.identity_id, &acct.id_s.public_bytes())
            .is_ok());
        assert!(manifest_covers(
            &new.header().manifest,
            &old.header().manifest
        ));
        assert!(!manifest_covers(
            &old.header().manifest,
            &new.header().manifest
        ));
        assert_ne!(
            manifest_digest(&old.header().manifest),
            manifest_digest(&new.header().manifest)
        );
    }

    #[test]
    fn the_manifest_digest_is_the_manifest_and_nothing_else() {
        let acct = account(1);
        let (d_s, cert) = device(&acct, 5);
        let one = decode_key_bundle(&seal(&acct, &d_s, &cert, &keys())).unwrap();
        let two = decode_key_bundle(&seal(&acct, &d_s, &cert, &keys())).unwrap();
        assert_eq!(
            manifest_digest(&one.header().manifest),
            manifest_digest(&manifest_of(&keys()))
        );
        assert_eq!(
            manifest_digest(&one.header().manifest),
            manifest_digest(&two.header().manifest)
        );
    }

    #[test]
    fn junk_and_other_kinds_are_refused() {
        assert!(matches!(
            decode_key_bundle(b"SR\x04\x00\x01junk"),
            Err(KeyBundleError::Malformed(_))
        ));
        assert!(matches!(
            decode_key_bundle(b"SR\x08\x00\x01\xa0"),
            Err(KeyBundleError::Malformed(_))
        ));
        assert!(matches!(
            decode_key_bundle(b"SR\x08\x00\x02\xa0"),
            Err(KeyBundleError::Malformed(_))
        ));
        assert!(matches!(
            decode_key_bundle(b""),
            Err(KeyBundleError::Malformed(_))
        ));
    }

    /// The same `(stream, epoch)` held twice — two concurrent mints — is two
    /// keys under one manifest line.
    #[test]
    fn two_keys_at_one_epoch_are_one_manifest_line() {
        let acct = account(1);
        let (d_s, cert) = device(&acct, 5);
        let mut k = keys();
        k.push(BundledKey {
            stream_id: [7; 16],
            epoch: 1,
            key: StreamKey::from_bytes([0x72; 32]),
        });
        let bundle = decode_key_bundle(&seal(&acct, &d_s, &cert, &k)).unwrap();
        assert!(bundle.header().manifest.contains(&ManifestEntry {
            stream_id: [7; 16],
            epoch: 1,
            keys: 2
        }));
        let opened = bundle
            .verify(&acct.identity_id, &acct.id_s.public_bytes())
            .unwrap()
            .open(&acct.id_d)
            .unwrap();
        assert_eq!(opened.len(), 4);
    }
}
