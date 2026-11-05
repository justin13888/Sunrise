//! The three-message pairing protocol: offer, request, grant.
//!
//! ```text
//!   sponsor (holds the vault)                joiner (has nothing)
//!   ------------------------------           --------------------------------
//!   1. PairingOffer            --------->    mints D_S / D_D
//!      account identity, public halves
//!      only; no secret of any kind
//!
//!                              <---------    2. PairingRequest
//!                                            device_id, D_S_pub, D_D_pub
//!
//!   3. PairingGrant            --------->    adopts the cert and the keys
//!      DeviceCert issued under ID_S,
//!      vault root, every Stream key
//! ```
//!
//! # Why three messages and not one
//!
//! Pairing used to be one message, and that message carried `ID_S_priv` — the
//! account's Ed25519 signing secret. It had to: a device that cannot sign under
//! the account identity cannot issue itself a `DeviceCert`, and without a cert
//! it is not a member of anything.
//!
//! The cost was [#105](https://github.com/justin13888/Sunrise/issues/105).
//! A `DeviceCert` names only its *subject* — `device_id`, `d_s_pub`, `d_d_pub`,
//! `identity_id` — and its single signature is the identity's, which on the old
//! protocol every paired device could produce. So a revoked device minted a
//! fresh `D_S`, derived a device id the revocation register had never heard of,
//! signed a cert for it that genuinely verified, and rejoined. Identity rotation
//! (ADR-0037) bounded that — the cert was valid under an identity the account
//! had retired — but only until the next revocation, when the device revoked
//! *that* time played the same trick under the new identity.
//!
//! Withholding the key closes it outright, and withholding it is what forces
//! three messages: **the sponsor cannot sign a cert for keys the joiner has not
//! minted yet.** The joiner must mint first and ask second. There is no one-shot
//! shape that works — see the module docs on [`crate::payload`] for the two that
//! were priced and rejected.
//!
//! # This is not ADR-0032's alternative 2
//!
//! ADR-0032 rejected *sponsor-countersigned* certs: a cert carrying a second
//! signature from the sponsoring device, which every verifier would have to
//! check. That binds a device's membership to its sponsor forever, so revoking
//! one device silently invalidates every device it ever paired — including, on a
//! chain of pairings, devices the user never associated with it.
//!
//! What this builds is sponsor-**issued**. The sponsor produces the signature
//! and then the signature is the *identity's*, exactly as it always was: byte
//! for byte the same `DeviceCert` shape, one signature, no issuer field, no
//! sponsor binding. A verifier cannot tell which device held `ID_S_priv` when
//! the cert was signed and does not need to. Revoking a sponsor therefore locks
//! nobody out. The sponsor is a gate at issue time and leaves no trace on the
//! artifact.
//!
//! # Transport-agnostic on purpose
//!
//! Nothing here knows whether a message crosses a Noise transport frame, a file
//! on disk, or a QR code. `sunrise-core-bindings` maps the three onto the
//! `PairedChannel` the Apple clients already drive; `sunrise-cli` maps them onto
//! three files. Both get the same bytes and the same checks.
//!
//! # What each side must check, and where
//!
//! Every field in every message is a value the *other* side chose, so each is
//! recomputed rather than believed:
//!
//! - [`decode_pairing_offer`] recomputes `identity_id` from `id_s_pub` and
//!   `genesis_identity_id` from `genesis_id_s_pub`, the same two checks the
//!   one-shot payload made.
//! - [`decode_pairing_request`] recomputes `device_id` from `d_s_pub`. A joiner
//!   that could name its own id would choose one the account's revocation
//!   register already excludes, or one that collides with a sibling's row.
//! - [`PairingGrant::accept`] is the joiner's check on the way back, and it is
//!   the one that matters: the cert must verify under the `ID_S_pub` the offer
//!   named, and must name *this* joiner's keys. A sponsor that issued a cert for
//!   somebody else's `D_S_pub` would hand the joiner a credential it cannot
//!   sign under, and the failure would not surface until the first op it tried
//!   to publish was refused by every peer.
//!
//! # Layout
//!
//! One file per message, `offer.rs`, `request.rs` and `grant.rs`, each holding
//! its type, its codec and its tests, and `joiner.rs` for [`PairingJoiner`],
//! the state between messages 2 and 3.
//! The fixtures they share, and the framing-depth test that spans every
//! message, live in this module's tests.

mod grant;
mod joiner;
mod offer;
mod request;

pub use grant::{decode_pairing_grant, PairingGrant};
pub use joiner::PairingJoiner;
pub use offer::{decode_pairing_offer, PairingOffer};
pub use request::{decode_pairing_request, PairingRequest};

/// The fixtures every message's tests share, and the one test that spans
/// every message.
#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::codec::{decode_message, int};
    use crate::payload::PairingPayloadError;
    use sunrise_crypto::keys::{
        DeviceDhKeyPair, DeviceSigningKeyPair, IdentityDhKeyPair, IdentitySigningKeyPair,
    };
    use sunrise_crypto::{device_id_from_pub, identity_id_from_pub, DeviceCert, DeviceCertInner};

    pub(super) fn sponsor_identity() -> IdentitySigningKeyPair {
        IdentitySigningKeyPair::from_secret_bytes(&[0x21; 32])
    }

    pub(super) fn offer() -> PairingOffer {
        let signing = sponsor_identity();
        let id_s_pub = signing.public_bytes();
        PairingOffer {
            id_s_pub,
            id_d_pub: IdentityDhKeyPair::from_secret_bytes([0x22; 32]).public_bytes(),
            identity_id: identity_id_from_pub(&id_s_pub),
            genesis_identity_id: identity_id_from_pub(&id_s_pub),
            genesis_id_s_pub: id_s_pub,
            nickname: "a laptop".into(),
            platform: "macos".into(),
        }
    }

    pub(super) fn joiner() -> (DeviceSigningKeyPair, DeviceDhKeyPair, PairingRequest) {
        let d_s = DeviceSigningKeyPair::from_secret_bytes(&[0x31; 32]);
        let d_d = DeviceDhKeyPair::from_secret_bytes([0x32; 32]);
        let request = PairingRequest {
            device_id: device_id_from_pub(&d_s.public_bytes()),
            d_s_pub: d_s.public_bytes(),
            d_d_pub: d_d.public_bytes(),
            identity_id: offer().identity_id,
            nickname: "a phone".into(),
            platform: "ios".into(),
        };
        (d_s, d_d, request)
    }

    pub(super) fn grant_for(
        request: &PairingRequest,
        signing: &IdentitySigningKeyPair,
    ) -> PairingGrant {
        let body = DeviceCertInner {
            v: 1,
            device_id: request.device_id,
            d_s_pub: request.d_s_pub,
            d_d_pub: request.d_d_pub,
            identity_id: identity_id_from_pub(&signing.public_bytes()),
            created_at_ms: 1_700_000_000_000,
            nickname: request.nickname.clone(),
            platform: request.platform.clone(),
        };
        let mut stream_keys = BTreeMap::new();
        stream_keys.insert([7u8; 16], BTreeMap::from([(1u32, [0x44u8; 32])]));
        PairingGrant {
            device_cert: DeviceCert::issue(body, signing).unwrap().to_cbor().unwrap(),
            vault_root: [0x25; 32],
            stream_keys,
            read_bounds: BTreeSet::from([[0xc1; 16]]),
        }
    }

    /// `bytes`, a CBOR map, with one more key no build knows: `levels`
    /// one-element arrays around an integer.
    fn with_unknown(bytes: &[u8], levels: usize) -> Vec<u8> {
        use ciborium::value::Value;
        let Value::Map(mut map) = decode_message(bytes).unwrap() else {
            panic!("a pairing message is a map");
        };
        let mut nest = Value::Integer(1.into());
        for _ in 0..levels {
            nest = Value::Array(vec![nest]);
        }
        map.push((int(99), nest));
        let mut out = Vec::new();
        ciborium::ser::into_writer(&Value::Map(map), &mut out).unwrap();
        out
    }

    /// Every message skips a key it does not know, as far as the framing
    /// bound and no further: nested to the bound with the message map it
    /// decodes, and one level more is refused as too deep.
    #[test]
    fn every_message_is_bounded_at_the_framing_depth() {
        let bound = sunrise_cbor::MAX_VALUE_DEPTH;
        let o = offer();
        let offer_bytes = o.encode().unwrap();
        assert_eq!(
            decode_pairing_offer(&with_unknown(&offer_bytes, bound - 1)).unwrap(),
            o
        );
        assert_eq!(
            decode_pairing_offer(&with_unknown(&offer_bytes, bound)),
            Err(PairingPayloadError::TooDeep)
        );

        let (_, _, r) = joiner();
        let request_bytes = r.encode().unwrap();
        assert_eq!(
            decode_pairing_request(&with_unknown(&request_bytes, bound - 1)).unwrap(),
            r
        );
        assert_eq!(
            decode_pairing_request(&with_unknown(&request_bytes, bound)),
            Err(PairingPayloadError::TooDeep)
        );

        let grant_bytes = grant_for(&r, &sponsor_identity()).encode().unwrap();
        assert!(decode_pairing_grant(&with_unknown(&grant_bytes, bound - 1)).is_ok());
        assert!(matches!(
            decode_pairing_grant(&with_unknown(&grant_bytes, bound)),
            Err(PairingPayloadError::TooDeep)
        ));

        let (d_s, d_d, request) = joiner();
        let payload = decode_pairing_grant(&grant_bytes)
            .unwrap()
            .accept(&offer(), d_s.secret_bytes(), d_d.secret_bytes(), &request)
            .unwrap();
        let payload_bytes = crate::payload::encode_pairing_payload(&payload).unwrap();
        let back = crate::payload::decode_pairing_payload(&with_unknown(&payload_bytes, bound - 1));
        assert_eq!(back.unwrap().vault_root, payload.vault_root);
        assert!(matches!(
            crate::payload::decode_pairing_payload(&with_unknown(&payload_bytes, bound)),
            Err(PairingPayloadError::TooDeep)
        ));

        // A deep nest far past the bound is refused as too deep, not as
        // truncated.
        assert_eq!(
            decode_pairing_offer(&[0x81; 4096]),
            Err(PairingPayloadError::TooDeep)
        );
    }
}
