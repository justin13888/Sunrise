//! Message 2, [`PairingRequest`]: the joiner's freshly minted public keys.

use subtle::ConstantTimeEq;
use sunrise_crypto::device_id_from_pub;

use crate::codec::{arr16, arr32, decode_message, int};
use crate::payload::{PairingPayloadError, MAX_PAIRING_PAYLOAD};

/// Message 2: the joiner's cert request.
///
/// Public halves only, again — the joiner keeps `D_S_priv` and `D_D_priv` and
/// never sends them anywhere. That asymmetry is what distinguishes this from the
/// rejected alternative where the *sponsor* mints the joiner's keypair: a sponsor
/// that minted the joiner's `D_S` would hold permanent impersonation of it, and
/// unlike `ID_S_priv` no rotation touches `D_S`, so revoking the sponsor would
/// not take the capability away. That is #105 wearing a different hat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingRequest {
    /// `device_id_from_pub(d_s_pub)`, recomputed by the decoder.
    pub device_id: [u8; 16],
    /// `D_S_pub` — the Ed25519 key every op this device writes is signed under.
    pub d_s_pub: [u8; 32],
    /// `D_D_pub` — the X25519 key every `key_envelope` addressed to this device
    /// is sealed to, and the one a revocation stops addressing.
    pub d_d_pub: [u8; 32],
    /// The identity the offer named, echoed back.
    ///
    /// The sponsor refuses a request that answers a different account's offer.
    /// Without it a request captured from one pairing could be replayed at a
    /// second sponsor, which would issue a cert for a device it never spoke to.
    pub identity_id: [u8; 16],
    /// The nickname the joiner wants, which the sponsor writes into the cert.
    pub nickname: String,
    /// The joiner's platform, likewise.
    pub platform: String,
}

impl PairingRequest {
    /// Encode to canonical, integer-keyed CBOR.
    ///
    /// ```cddl
    /// PairingRequest = {
    ///     1: bstr .size 16,   ; device_id
    ///     2: bstr .size 32,   ; D_S_pub
    ///     3: bstr .size 32,   ; D_D_pub
    ///     4: bstr .size 16,   ; identity_id the offer named
    ///     5: tstr,            ; nickname
    ///     6: tstr,            ; platform
    /// }
    /// ```
    ///
    /// # Errors
    /// [`PairingPayloadError::Cbor`] on an encode failure.
    pub fn encode(&self) -> Result<Vec<u8>, PairingPayloadError> {
        use ciborium::value::Value;
        let map = vec![
            (int(1), Value::Bytes(self.device_id.to_vec())),
            (int(2), Value::Bytes(self.d_s_pub.to_vec())),
            (int(3), Value::Bytes(self.d_d_pub.to_vec())),
            (int(4), Value::Bytes(self.identity_id.to_vec())),
            (int(5), Value::Text(self.nickname.clone())),
            (int(6), Value::Text(self.platform.clone())),
        ];
        let mut out = Vec::with_capacity(192);
        ciborium::ser::into_writer(&Value::Map(map), &mut out)
            .map_err(|e| PairingPayloadError::Cbor(e.to_string()))?;
        Ok(out)
    }
}

/// Decode and validate a [`PairingRequest`].
///
/// `device_id` is recomputed from `d_s_pub`. It is the one field a joiner could
/// otherwise choose freely, and choosing it is how a device would claim an id
/// the account has already revoked or one that collides with a sibling's row.
///
/// # Errors
/// [`PairingPayloadError::Cbor`], [`PairingPayloadError::BadField`], or
/// [`PairingPayloadError::DeviceIdMismatch`].
pub fn decode_pairing_request(bytes: &[u8]) -> Result<PairingRequest, PairingPayloadError> {
    use ciborium::value::Value;

    if bytes.len() > MAX_PAIRING_PAYLOAD {
        return Err(PairingPayloadError::TooLarge { len: bytes.len() });
    }
    let Value::Map(map) = decode_message(bytes)? else {
        return Err(PairingPayloadError::Cbor("request must be a map".into()));
    };

    let mut device_id = None;
    let mut d_s_pub = None;
    let mut d_d_pub = None;
    let mut identity_id = None;
    let mut nickname = None;
    let mut platform = None;

    for (k, v) in map {
        let Value::Integer(i) = k else {
            return Err(PairingPayloadError::Cbor("non-int key".into()));
        };
        match (i128::from(i), v) {
            (1, Value::Bytes(b)) => device_id = Some(arr16(&b, "request device_id")?),
            (2, Value::Bytes(b)) => d_s_pub = Some(arr32(&b, "request d_s_pub")?),
            (3, Value::Bytes(b)) => d_d_pub = Some(arr32(&b, "request d_d_pub")?),
            (4, Value::Bytes(b)) => identity_id = Some(arr16(&b, "request identity_id")?),
            (5, Value::Text(s)) => nickname = Some(s),
            (6, Value::Text(s)) => platform = Some(s),
            (id, _) if (1..=6).contains(&id) => {
                return Err(PairingPayloadError::BadField("request field shape"));
            }
            _ => {}
        }
    }

    let request = PairingRequest {
        device_id: device_id.ok_or(PairingPayloadError::BadField("request device_id"))?,
        d_s_pub: d_s_pub.ok_or(PairingPayloadError::BadField("request d_s_pub"))?,
        d_d_pub: d_d_pub.ok_or(PairingPayloadError::BadField("request d_d_pub"))?,
        identity_id: identity_id.ok_or(PairingPayloadError::BadField("request identity_id"))?,
        nickname: nickname.ok_or(PairingPayloadError::BadField("request nickname"))?,
        platform: platform.ok_or(PairingPayloadError::BadField("request platform"))?,
    };
    if !bool::from(device_id_from_pub(&request.d_s_pub).ct_eq(&request.device_id)) {
        return Err(PairingPayloadError::DeviceIdMismatch);
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::tests::joiner;

    #[test]
    fn the_request_round_trips() {
        let (_, _, r) = joiner();
        assert_eq!(decode_pairing_request(&r.encode().unwrap()).unwrap(), r);
    }

    /// A joiner does not get to name its own device id. Choosing it is how a
    /// device would claim an id the revocation register has no row for, or one
    /// that collides with a sibling.
    #[test]
    fn a_request_naming_a_device_id_its_key_does_not_derive_is_refused() {
        let (_, _, mut r) = joiner();
        r.device_id = [0x99; 16];
        assert!(matches!(
            decode_pairing_request(&r.encode().unwrap()),
            Err(PairingPayloadError::DeviceIdMismatch)
        ));
    }
}
