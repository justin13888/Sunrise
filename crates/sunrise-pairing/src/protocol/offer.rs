//! Message 1, [`PairingOffer`]: the sponsor's account identity, public halves
//! only.

use subtle::ConstantTimeEq;
use sunrise_crypto::identity_id_from_pub;

use crate::codec::{arr16, arr32, decode_message, int};
use crate::payload::{PairingPayloadError, MAX_PAIRING_PAYLOAD};

/// Message 1: what the sponsor offers, before it has been asked for anything.
///
/// **Carries no secret.** Every field is a public half or a derivation of one,
/// which is the whole point: a joiner that abandons the pairing here, or an
/// attacker that intercepts this message, has learned nothing it could not have
/// learned from any op the account ever published. The vault root and the Stream
/// keys do not travel until the sponsor has seen and accepted a request.
///
/// It is still sent through the confirmed channel. Not because the contents are
/// secret but because the *binding* is: a joiner that accepted an offer from
/// somebody else would mint keys for a stranger's account and wait forever for a
/// grant that never comes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingOffer {
    /// `ID_S_pub` of the identity **in force**, which is what the joiner's cert
    /// will be signed under and what it verifies that cert against.
    pub id_s_pub: [u8; 32],
    /// `ID_D_pub`. Sealing a `key_envelope` to the identity needs only this
    /// half; `ID_D_priv` has not travelled since `#76` and still does not.
    pub id_d_pub: [u8; 32],
    /// The identity id in force — `identity_id_from_pub(id_s_pub)`, recomputed
    /// by the decoder.
    pub identity_id: [u8; 16],
    /// The identity the account **started** as, which no rotation changes.
    ///
    /// The fold anchor (`identity.genesis_identity_id`). Equal to `identity_id`
    /// on an account that has never rotated and carried anyway: "equal unless
    /// rotated" is a rule the receiver has no way to evaluate.
    pub genesis_identity_id: [u8; 16],
    /// `ID_S_pub` of the genesis identity, so the first link of the chain — which
    /// is nobody's successor and so appears in no transition row — has a key its
    /// signature can be checked under.
    pub genesis_id_s_pub: [u8; 32],
    /// Nickname of the sponsoring device, for a screen that wants to say which
    /// device is admitting this one.
    pub nickname: String,
    /// Platform of the sponsoring device.
    pub platform: String,
}

impl PairingOffer {
    /// Encode to canonical, integer-keyed CBOR.
    ///
    /// ```cddl
    /// PairingOffer = {
    ///     1: bstr .size 32,   ; ID_S_pub
    ///     2: bstr .size 32,   ; ID_D_pub
    ///     3: bstr .size 16,   ; identity_id
    ///     4: bstr .size 16,   ; genesis_identity_id
    ///     5: bstr .size 32,   ; genesis ID_S_pub
    ///     6: tstr,            ; sponsor nickname
    ///     7: tstr,            ; sponsor platform
    /// }
    /// ```
    ///
    /// # Errors
    /// [`PairingPayloadError::Cbor`] on an encode failure.
    pub fn encode(&self) -> Result<Vec<u8>, PairingPayloadError> {
        use ciborium::value::Value;
        let map = vec![
            (int(1), Value::Bytes(self.id_s_pub.to_vec())),
            (int(2), Value::Bytes(self.id_d_pub.to_vec())),
            (int(3), Value::Bytes(self.identity_id.to_vec())),
            (int(4), Value::Bytes(self.genesis_identity_id.to_vec())),
            (int(5), Value::Bytes(self.genesis_id_s_pub.to_vec())),
            (int(6), Value::Text(self.nickname.clone())),
            (int(7), Value::Text(self.platform.clone())),
        ];
        let mut out = Vec::with_capacity(256);
        ciborium::ser::into_writer(&Value::Map(map), &mut out)
            .map_err(|e| PairingPayloadError::Cbor(e.to_string()))?;
        Ok(out)
    }
}

/// Decode and validate a [`PairingOffer`].
///
/// Both id/key pairs are recomputed before the value is handed back, so a caller
/// cannot forget to. Both comparisons are constant-time: the attacker is on the
/// far side of a SAS-confirmed channel and the signal is small, but a documented
/// property the code does not have is worse than not claiming it.
///
/// # Errors
/// [`PairingPayloadError::Cbor`] for malformed CBOR,
/// [`PairingPayloadError::BadField`] for a missing or wrongly-shaped field,
/// [`PairingPayloadError::IdentityMismatch`] and
/// [`PairingPayloadError::GenesisMismatch`] when an id and its key disagree.
pub fn decode_pairing_offer(bytes: &[u8]) -> Result<PairingOffer, PairingPayloadError> {
    use ciborium::value::Value;

    if bytes.len() > MAX_PAIRING_PAYLOAD {
        return Err(PairingPayloadError::TooLarge { len: bytes.len() });
    }
    let Value::Map(map) = decode_message(bytes)? else {
        return Err(PairingPayloadError::Cbor("offer must be a map".into()));
    };

    let mut id_s_pub = None;
    let mut id_d_pub = None;
    let mut identity_id = None;
    let mut genesis_identity_id = None;
    let mut genesis_id_s_pub = None;
    let mut nickname = None;
    let mut platform = None;

    for (k, v) in map {
        let Value::Integer(i) = k else {
            return Err(PairingPayloadError::Cbor("non-int key".into()));
        };
        match (i128::from(i), v) {
            (1, Value::Bytes(b)) => id_s_pub = Some(arr32(&b, "offer id_s_pub")?),
            (2, Value::Bytes(b)) => id_d_pub = Some(arr32(&b, "offer id_d_pub")?),
            (3, Value::Bytes(b)) => identity_id = Some(arr16(&b, "offer identity_id")?),
            (4, Value::Bytes(b)) => {
                genesis_identity_id = Some(arr16(&b, "offer genesis_identity_id")?);
            }
            (5, Value::Bytes(b)) => genesis_id_s_pub = Some(arr32(&b, "offer genesis_id_s_pub")?),
            (6, Value::Text(s)) => nickname = Some(s),
            (7, Value::Text(s)) => platform = Some(s),
            // The reserved range. A value in it that matched no arm above is a
            // field we know wearing the wrong CBOR type, which is an error
            // rather than something to skip. The `_` arm below is forward
            // compatibility and nothing else.
            (id, _) if (1..=7).contains(&id) => {
                return Err(PairingPayloadError::BadField("offer field shape"));
            }
            _ => {}
        }
    }

    let offer = PairingOffer {
        id_s_pub: id_s_pub.ok_or(PairingPayloadError::BadField("offer id_s_pub"))?,
        id_d_pub: id_d_pub.ok_or(PairingPayloadError::BadField("offer id_d_pub"))?,
        identity_id: identity_id.ok_or(PairingPayloadError::BadField("offer identity_id"))?,
        genesis_identity_id: genesis_identity_id
            .ok_or(PairingPayloadError::BadField("offer genesis_identity_id"))?,
        genesis_id_s_pub: genesis_id_s_pub
            .ok_or(PairingPayloadError::BadField("offer genesis_id_s_pub"))?,
        nickname: nickname.ok_or(PairingPayloadError::BadField("offer nickname"))?,
        platform: platform.ok_or(PairingPayloadError::BadField("offer platform"))?,
    };

    if !bool::from(identity_id_from_pub(&offer.genesis_id_s_pub).ct_eq(&offer.genesis_identity_id))
    {
        return Err(PairingPayloadError::GenesisMismatch);
    }
    if !bool::from(identity_id_from_pub(&offer.id_s_pub).ct_eq(&offer.identity_id)) {
        return Err(PairingPayloadError::IdentityMismatch);
    }
    Ok(offer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::tests::{offer, sponsor_identity};

    #[test]
    fn the_offer_round_trips() {
        let o = offer();
        let back = decode_pairing_offer(&o.encode().unwrap()).unwrap();
        assert_eq!(back, o);
    }

    /// The offer is the one message that carries nothing worth stealing, and
    /// that is a property rather than an accident. Asserted on the *bytes*,
    /// because a field added later that happened to be a secret would still
    /// encode fine and this is what would catch it.
    #[test]
    fn the_offer_carries_no_secret() {
        let signing = sponsor_identity();
        let bytes = offer().encode().unwrap();
        let windows_contain = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
        assert!(
            !windows_contain(&signing.secret_bytes()),
            "ID_S_priv must not be anywhere in a pairing offer"
        );
        assert!(
            !windows_contain(&[0x22u8; 32]),
            "ID_D_priv must not be anywhere in a pairing offer"
        );
    }

    #[test]
    fn an_offer_whose_identity_id_is_rewritten_is_refused() {
        let mut o = offer();
        o.identity_id = [0xff; 16];
        assert!(matches!(
            decode_pairing_offer(&o.encode().unwrap()),
            Err(PairingPayloadError::IdentityMismatch)
        ));
    }

    #[test]
    fn an_offer_whose_genesis_anchor_is_rewritten_is_refused() {
        let mut o = offer();
        o.genesis_identity_id = [0xff; 16];
        assert!(matches!(
            decode_pairing_offer(&o.encode().unwrap()),
            Err(PairingPayloadError::GenesisMismatch)
        ));
    }
}
