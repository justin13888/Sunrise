//! Message 3, [`PairingGrant`]: the cert the sponsor issued, the vault root and
//! every Stream key, and the joiner's check of them.

use std::collections::{BTreeMap, BTreeSet};

use subtle::ConstantTimeEq;
use sunrise_crypto::DeviceCert;
use zeroize::Zeroize;

use super::{PairingOffer, PairingRequest};
use crate::codec::{
    arr32, decode_message, int, read_bounds_value, read_read_bounds, read_stream_keys,
};
use crate::payload::{PairingPayload, PairingPayloadError, MAX_PAIRING_PAYLOAD};

/// Message 3: the sponsor's grant.
///
/// This is the message that carries everything worth carrying: the issued cert,
/// the vault root, and every Stream key the sponsor holds. It zeroizes on drop
/// for the same reason the assembled [`PairingPayload`] does.
#[derive(Clone)]
pub struct PairingGrant {
    /// The `DeviceCert` the sponsor issued for the joiner, canonical CBOR.
    ///
    /// Signed by `ID_S_priv`, which the joiner does not have and will never
    /// have. One signature; no countersignature; no issuer field.
    pub device_cert: Vec<u8>,
    /// The vault root, which keys the local database and wraps everything at
    /// rest.
    pub vault_root: [u8; 32],
    /// Every Stream key the sponsoring device holds: `stream_id -> epoch -> key`.
    pub stream_keys: BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>>,
    /// Every device the sponsor has read-bounded, which the joiner adopts.
    /// See [`PairingPayload::read_bounds`] for why the bound travels and the
    /// ledger does not.
    pub read_bounds: BTreeSet<[u8; 16]>,
}

impl Zeroize for PairingGrant {
    /// Hand-written because neither `Vec<u8>`'s capacity nor `BTreeMap`'s nodes
    /// are scrubbed by dropping the container.
    fn zeroize(&mut self) {
        self.device_cert.zeroize();
        self.vault_root.zeroize();
        for epochs in self.stream_keys.values_mut() {
            for key in epochs.values_mut() {
                key.zeroize();
            }
        }
        self.stream_keys.clear();
    }
}

impl Drop for PairingGrant {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl std::fmt::Debug for PairingGrant {
    /// Names the shape and nothing else, like [`PairingPayload`]'s.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingGrant")
            .field("streams", &self.stream_keys.len())
            .finish_non_exhaustive()
    }
}

impl PairingGrant {
    /// Encode to canonical, integer-keyed CBOR.
    ///
    /// ```cddl
    /// PairingGrant = {
    ///     1: bstr,            ; DeviceCert, canonical CBOR
    ///     2: bstr .size 32,   ; vault_root
    ///     3: { * bstr .size 16 => { * uint => bstr .size 32 } },  ; stream_keys
    ///   ? 4: [ * bstr .size 16 ],  ; read_bounds, omitted when empty
    /// }
    /// ```
    ///
    /// # Errors
    /// [`PairingPayloadError::Cbor`] on an encode failure, or
    /// [`PairingPayloadError::TooLarge`] if the result will not fit in one Noise
    /// transport message. Surfaced rather than truncated: a truncated grant is a
    /// device that silently cannot read half its own account.
    pub fn encode(&self) -> Result<Vec<u8>, PairingPayloadError> {
        use ciborium::value::{Integer, Value};
        let mut streams: Vec<(Value, Value)> = Vec::with_capacity(self.stream_keys.len());
        for (stream_id, epochs) in &self.stream_keys {
            let mut per_epoch: Vec<(Value, Value)> = Vec::with_capacity(epochs.len());
            for (epoch, key) in epochs {
                per_epoch.push((
                    Value::Integer(Integer::from(*epoch)),
                    Value::Bytes(key.to_vec()),
                ));
            }
            streams.push((Value::Bytes(stream_id.to_vec()), Value::Map(per_epoch)));
        }
        let mut map = vec![
            (int(1), Value::Bytes(self.device_cert.clone())),
            (int(2), Value::Bytes(self.vault_root.to_vec())),
            (int(3), Value::Map(streams)),
        ];
        if let Some(bounds) = read_bounds_value(&self.read_bounds) {
            map.push((int(4), bounds));
        }
        let mut out = Vec::with_capacity(512);
        ciborium::ser::into_writer(&Value::Map(map), &mut out)
            .map_err(|e| PairingPayloadError::Cbor(e.to_string()))?;
        if out.len() > MAX_PAIRING_PAYLOAD {
            let len = out.len();
            out.zeroize();
            return Err(PairingPayloadError::TooLarge { len });
        }
        Ok(out)
    }

    /// Total `(stream, epoch)` pairs carried.
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.stream_keys.values().map(BTreeMap::len).sum()
    }

    /// Turn an accepted grant into the material [`PairingPayload`] carries,
    /// checking that the cert is one this device can actually use.
    ///
    /// This is the joiner's only opportunity to refuse. Everything it checks is
    /// something a hostile or merely broken sponsor could get wrong, and each
    /// failure would otherwise surface much later as a device whose ops every
    /// peer silently rejects:
    ///
    /// - the cert parses, and **verifies under the `ID_S_pub` the offer named**.
    ///   Not under the cert's own claim of an identity: `verify_binding`
    ///   recomputes `identity_id` from the key and compares it to the one the
    ///   offer committed to, so a cert signed by a well-formed identity that is
    ///   not this account's is refused.
    /// - the cert names *this* joiner's `d_s_pub`, `d_d_pub` and derived
    ///   `device_id`. A sponsor that certified a different device's keys would
    ///   hand over a credential the joiner holds no signing key for.
    ///
    /// `d_s_priv` and `d_d_priv` are the secrets the joiner minted for
    /// [`PairingRequest`] and never sent.
    ///
    /// # Errors
    /// [`PairingPayloadError::BadCert`] when the cert does not parse, does not
    /// verify under the offer's identity, or names keys other than these.
    pub fn accept(
        self,
        offer: &PairingOffer,
        d_s_priv: [u8; 32],
        d_d_priv: [u8; 32],
        request: &PairingRequest,
    ) -> Result<PairingPayload, PairingPayloadError> {
        let cert = DeviceCert::from_cbor(&self.device_cert)
            .map_err(|_| PairingPayloadError::BadCert("granted cert does not decode"))?;
        cert.verify_binding(&offer.id_s_pub, &offer.identity_id)
            .map_err(|_| {
                PairingPayloadError::BadCert("granted cert is not signed by this account identity")
            })?;
        // Constant-time, like every other comparison on this path, and for the
        // same reason: these run on bytes the peer chose.
        let subject_ok = cert.body.d_s_pub.ct_eq(&request.d_s_pub)
            & cert.body.d_d_pub.ct_eq(&request.d_d_pub)
            & cert.body.device_id.ct_eq(&request.device_id);
        if !bool::from(subject_ok) {
            return Err(PairingPayloadError::BadCert(
                "granted cert names a different device",
            ));
        }
        // Moved out field by field so `PairingGrant`'s `Drop` does not scrub
        // values that are now the payload's.
        let mut grant = self;
        let device_cert = std::mem::take(&mut grant.device_cert);
        let vault_root = grant.vault_root;
        let stream_keys = std::mem::take(&mut grant.stream_keys);
        let read_bounds = std::mem::take(&mut grant.read_bounds);
        grant.vault_root.zeroize();
        Ok(PairingPayload {
            id_s_pub: offer.id_s_pub,
            id_d_pub: offer.id_d_pub,
            identity_id: offer.identity_id,
            genesis_identity_id: offer.genesis_identity_id,
            genesis_id_s_pub: offer.genesis_id_s_pub,
            d_s_priv,
            d_d_priv,
            device_cert,
            vault_root,
            stream_keys,
            read_bounds,
        })
    }
}

/// Decode a [`PairingGrant`].
///
/// Nothing here can be validated in isolation — a cert means nothing without the
/// offer's `ID_S_pub` and the request's keys to check it against — so the
/// checking lives in [`PairingGrant::accept`], which has all three.
///
/// # Errors
/// [`PairingPayloadError::Cbor`], [`PairingPayloadError::BadField`], or
/// [`PairingPayloadError::TooLarge`].
pub fn decode_pairing_grant(bytes: &[u8]) -> Result<PairingGrant, PairingPayloadError> {
    use ciborium::value::Value;

    if bytes.len() > MAX_PAIRING_PAYLOAD {
        return Err(PairingPayloadError::TooLarge { len: bytes.len() });
    }
    let Value::Map(map) = decode_message(bytes)? else {
        return Err(PairingPayloadError::Cbor("grant must be a map".into()));
    };

    let mut device_cert: Option<Vec<u8>> = None;
    let mut vault_root: Option<[u8; 32]> = None;
    let mut stream_keys: BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>> = BTreeMap::new();
    let mut read_bounds: BTreeSet<[u8; 16]> = BTreeSet::new();

    for (k, v) in map {
        let Value::Integer(i) = k else {
            return Err(PairingPayloadError::Cbor("non-int key".into()));
        };
        match (i128::from(i), v) {
            (1, Value::Bytes(b)) => device_cert = Some(b),
            (2, Value::Bytes(b)) => vault_root = Some(arr32(&b, "grant vault_root")?),
            (3, Value::Map(streams)) => {
                read_stream_keys(streams, &mut stream_keys)?;
            }
            // Optional, and absent from a sponsor that predates it: the joiner
            // then adopts no bound, which is where every joiner used to start.
            (4, Value::Array(ids)) => read_read_bounds(ids, &mut read_bounds)?,
            (id, _) if (1..=4).contains(&id) => {
                return Err(PairingPayloadError::BadField("grant field shape"));
            }
            _ => {}
        }
    }

    Ok(PairingGrant {
        device_cert: device_cert.ok_or(PairingPayloadError::BadField("grant device_cert"))?,
        vault_root: vault_root.ok_or(PairingPayloadError::BadField("grant vault_root"))?,
        stream_keys,
        read_bounds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::tests::{grant_for, joiner, offer, sponsor_identity};
    use sunrise_crypto::device_id_from_pub;
    use sunrise_crypto::keys::{DeviceSigningKeyPair, IdentitySigningKeyPair};

    #[test]
    fn the_grant_round_trips_and_accepts() {
        let (d_s, d_d, request) = joiner();
        let g = grant_for(&request, &sponsor_identity());
        let bytes = g.encode().unwrap();
        let back = decode_pairing_grant(&bytes).unwrap();
        assert_eq!(back.key_count(), 1);
        let payload = back
            .accept(&offer(), d_s.secret_bytes(), d_d.secret_bytes(), &request)
            .expect("an honest grant is accepted");
        assert_eq!(payload.vault_root, [0x25; 32]);
        assert_eq!(payload.d_s_priv, d_s.secret_bytes());
        assert_eq!(
            payload.read_bounds,
            BTreeSet::from([[0xc1; 16]]),
            "the sponsor's read bound reaches the payload the joiner opens with"
        );
    }

    /// A grant from a sponsor that predates field 4 carries no bound, and the
    /// joiner accepts it with an empty one rather than refusing to pair.
    #[test]
    fn a_grant_without_read_bounds_decodes_with_none() {
        let (_, _, request) = joiner();
        let mut g = grant_for(&request, &sponsor_identity());
        g.read_bounds.clear();
        let bytes = g.encode().unwrap();
        assert!(decode_pairing_grant(&bytes).unwrap().read_bounds.is_empty());
    }

    /// The check that makes a second sponsor useless. A request replayed at a
    /// stranger's vault gets a cert signed by *that* account's identity, and the
    /// joiner refuses it because the offer it holds names a different key.
    #[test]
    fn a_cert_signed_by_another_identity_is_refused() {
        let (d_s, d_d, request) = joiner();
        let stranger = IdentitySigningKeyPair::from_secret_bytes(&[0x99; 32]);
        let g = grant_for(&request, &stranger);
        let err = g
            .accept(&offer(), d_s.secret_bytes(), d_d.secret_bytes(), &request)
            .expect_err("a cert from another identity must be refused");
        assert!(matches!(err, PairingPayloadError::BadCert(_)));
    }

    /// A sponsor that certifies somebody else's keys hands over a credential
    /// this device cannot sign under. Caught here rather than at the first
    /// rejected op.
    #[test]
    fn a_cert_naming_another_device_is_refused() {
        let (d_s, d_d, request) = joiner();
        let other = DeviceSigningKeyPair::from_secret_bytes(&[0x51; 32]);
        let mut wrong = request.clone();
        wrong.d_s_pub = other.public_bytes();
        wrong.device_id = device_id_from_pub(&other.public_bytes());
        let g = grant_for(&wrong, &sponsor_identity());
        let err = g
            .accept(&offer(), d_s.secret_bytes(), d_d.secret_bytes(), &request)
            .expect_err("a cert for another device must be refused");
        assert!(matches!(err, PairingPayloadError::BadCert(_)));
    }

    #[test]
    fn a_grant_missing_its_cert_is_refused() {
        use ciborium::value::Value;
        let (_, _, request) = joiner();
        let bytes = grant_for(&request, &sponsor_identity()).encode().unwrap();
        let Value::Map(mut map) = ciborium::de::from_reader::<Value, _>(bytes.as_slice()).unwrap()
        else {
            unreachable!()
        };
        map.retain(|(k, _)| !matches!(k, Value::Integer(i) if i128::from(*i) == 1));
        let mut trimmed = Vec::new();
        ciborium::ser::into_writer(&Value::Map(map), &mut trimmed).unwrap();
        assert!(matches!(
            decode_pairing_grant(&trimmed),
            Err(PairingPayloadError::BadField("grant device_cert"))
        ));
    }

    #[test]
    fn grant_debug_does_not_leak() {
        let (_, _, request) = joiner();
        let g = grant_for(&request, &sponsor_identity());
        let rendered = format!("{g:?}");
        assert!(!rendered.contains(&hex::encode(g.vault_root)));
    }
}
